//! The contract of the three traits, exercised with the smallest operators that have one.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{Cancel, Cause, ErrorCode, LogicalType, Value, slow};
use rudb_metrics::{Counters, thread_cpu_ns};
use rudb_vector::{Chunk, Vector};

use crate::dynamic::{DynSink, DynStream, LocalState};
use crate::morsel::Morsel;
use crate::parallel::run_parallel;
use crate::pipeline::Pipeline;
use crate::pool::Pool;
use crate::progress::{Blocked, BlockedReason, BufferId, IoToken, PipelineId, Progress};
use crate::root::{root, root_in_order};
use crate::serial::run_serial;
use crate::traits::{Sink, Source, Stream};
use crate::watch::Watched;

/// A source over a list of integers, handing out one morsel per group of `per_morsel` values and
/// reading them out `per_chunk` at a time, so that the repeated read of one morsel is exercised
/// rather than assumed.
#[derive(Debug)]
struct Counting {
    values: Vec<i64>,
    per_morsel: u64,
    per_chunk: u64,
    next: AtomicU64,
    reads: AtomicUsize,
}

impl Counting {
    fn new(values: Vec<i64>, per_morsel: u64, per_chunk: u64) -> Self {
        Self { values, per_morsel, per_chunk, next: AtomicU64::new(0), reads: AtomicUsize::new(0) }
    }
}

impl Source for Counting {
    fn morsels(&self, _threads: usize) -> Option<usize> {
        Some(usize::try_from((self.values.len() as u64).div_ceil(self.per_morsel)).unwrap_or(1))
    }

    fn morsel(&self) -> Option<Morsel> {
        let total = self.values.len() as u64;
        let start = self.next.fetch_add(self.per_morsel, Ordering::Relaxed);
        if start >= total {
            return None;
        }
        let end = total.min(start + self.per_morsel);
        Some(Morsel::new(start / self.per_morsel, start, end))
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> rudb_common::Result<Progress> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let take = self.per_chunk.min(morsel.remaining());
        let from = morsel.cursor() as usize;
        let slice = &self.values[from..from + take as usize];
        let values: Vec<Value> = slice.iter().map(|v| Value::BigInt(*v)).collect();
        *out = Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &values)?])?;
        morsel.advance(take);
        Ok(if morsel.is_drained() { Progress::Done } else { Progress::More })
    }
}

/// A source that remembers what it was told, so a test can say it was told at all.
///
/// Counting the calls as well as the number, because being asked twice and being asked once are
/// different things to a source that cuts its work when it answers.
#[derive(Debug, Default)]
struct Asked {
    told: AtomicUsize,
    calls: AtomicUsize,
}

impl Source for Asked {
    fn morsels(&self, threads: usize) -> Option<usize> {
        self.told.store(threads, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        Some(64)
    }

    fn morsel(&self) -> Option<Morsel> {
        None
    }

    fn read(&self, _morsel: &mut Morsel, out: &mut Chunk) -> rudb_common::Result<Progress> {
        *out = Chunk::empty(&[LogicalType::BigInt]);
        Ok(Progress::Done)
    }
}

/// Keeps rows whose value is even, by rebuilding the chunk. F0 is allowed to be slow.
#[derive(Debug)]
struct Evens;

impl Stream for Evens {
    type Local = usize;

    fn local(&self) -> usize {
        0
    }

    fn push(&self, chunk: &mut Chunk, seen: &mut usize) -> rudb_common::Result<Progress> {
        *seen += chunk.len();
        let kept: Vec<Value> = (0..chunk.len())
            .map(|row| chunk.value_at(row, 0))
            .filter(|value| matches!(value, Value::BigInt(v) if v % 2 == 0))
            .collect();
        *chunk = Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &kept)?])?;
        Ok(Progress::More)
    }
}

/// Hands every chunk it is given on `copies` times, which is the smallest operator whose one input
/// chunk is several output chunks. A cross product is this with the other side of the join in it.
#[derive(Debug)]
struct Repeating {
    copies: usize,
}

/// The chunk a [`Repeating`] is in the middle of, and how many copies of it are left.
#[derive(Debug, Default)]
struct Repeat {
    held: Option<Chunk>,
    left: usize,
}

impl Stream for Repeating {
    type Local = Repeat;

    fn local(&self) -> Repeat {
        Repeat::default()
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Repeat) -> rudb_common::Result<Progress> {
        let held = match local.held.take() {
            // Being asked again, so what is in the chunk is whatever was downstream of it.
            Some(held) => held,
            None => {
                local.left = self.copies;
                chunk.clone()
            }
        };
        local.left -= 1;
        *chunk = held.clone();
        if local.left == 0 {
            return Ok(Progress::More);
        }
        local.held = Some(held);
        Ok(Progress::Again)
    }
}

/// Stops the pipeline once it has seen `limit` rows go past.
#[derive(Debug)]
struct StopAfter {
    limit: usize,
}

impl Stream for StopAfter {
    type Local = usize;

    fn local(&self) -> usize {
        0
    }

    fn push(&self, chunk: &mut Chunk, seen: &mut usize) -> rudb_common::Result<Progress> {
        *seen += chunk.len();
        Ok(if *seen >= self.limit { Progress::Done } else { Progress::More })
    }
}

/// Sums what it is given, one running total per instance, merged at the end.
#[derive(Debug, Default)]
struct Total {
    global: Mutex<i64>,
    combines: AtomicUsize,
    finalizes: AtomicUsize,
}

impl Sink for Total {
    type Local = i64;

    fn local(&self) -> i64 {
        0
    }

    fn sink(&self, chunk: &Chunk, running: &mut i64) -> rudb_common::Result<Progress> {
        // row at a time: a test operator summing ten values, where a kernel would be more code than
        // the thing it tests
        for row in 0..chunk.len() {
            if let Value::BigInt(value) = chunk.value_at(row, 0) {
                *running += value;
            }
        }
        Ok(Progress::More)
    }

    fn combine(&self, local: i64) -> rudb_common::Result<()> {
        self.combines.fetch_add(1, Ordering::Relaxed);
        *self.global.lock().unwrap() += local;
        Ok(())
    }

    fn finalize(&self, _threads: &crate::Lease<'_>) -> rudb_common::Result<()> {
        self.finalizes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// A source that says it is waiting for a read it never issued.
#[derive(Debug)]
struct AlwaysBlocked;

impl Source for AlwaysBlocked {
    fn morsel(&self) -> Option<Morsel> {
        Some(Morsel::new(0, 0, 1))
    }

    fn read(&self, _morsel: &mut Morsel, _out: &mut Chunk) -> rudb_common::Result<Progress> {
        Ok(Progress::Blocked(Blocked::Io(IoToken(7))))
    }
}

fn pipeline(source: Arc<dyn Source>, sink: Arc<Total>) -> Pipeline<'static> {
    Pipeline::new(PipelineId(0), source, sink as Arc<dyn DynSink>)
}

#[test]
fn a_pipeline_with_one_source_and_one_sink_runs() {
    let source = Arc::new(Counting::new((1..=10).collect(), 4, 4));
    let sink = Arc::new(Total::default());
    let built = pipeline(source, Arc::clone(&sink));

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 55);
    assert_eq!(sink.combines.load(Ordering::Relaxed), 1);
    assert_eq!(sink.finalizes.load(Ordering::Relaxed), 1);
}

#[test]
fn one_morsel_is_read_many_times_when_it_is_wider_than_a_chunk() {
    let source = Arc::new(Counting::new((1..=12).collect(), 12, 3));
    let sink = Arc::new(Total::default());
    let built = pipeline(Arc::clone(&source) as Arc<dyn Source>, Arc::clone(&sink));

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 78);
    assert_eq!(source.reads.load(Ordering::Relaxed), 4);
}

#[test]
fn a_stream_runs_between_the_source_and_the_sink() {
    let source = Arc::new(Counting::new((1..=10).collect(), 10, 10));
    let sink = Arc::new(Total::default());
    let built = pipeline(source, Arc::clone(&sink)).then(Arc::new(Evens) as Arc<dyn DynStream>);

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 30);
}

#[test]
fn streams_run_in_the_order_they_were_added() {
    let source = Arc::new(Counting::new((1..=10).collect(), 10, 10));
    let sink = Arc::new(Total::default());
    let built = pipeline(source, Arc::clone(&sink))
        .then(Arc::new(Evens) as Arc<dyn DynStream>)
        .then(Arc::new(Evens) as Arc<dyn DynStream>);

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 30);
}

#[test]
fn a_stream_with_more_output_than_input_is_asked_again_for_the_same_chunk() {
    let source = Arc::new(Counting::new((1..=10).collect(), 10, 10));
    let sink = Arc::new(Total::default());
    let built =
        pipeline(source, Arc::clone(&sink)).then(Arc::new(Repeating { copies: 3 }) as Arc<_>);

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 165);
}

/// Two of them stacked, which is what makes the resume an order rather than a flag. The one nearest
/// the sink finishes its copies before the one below it produces its next one, and the total only
/// comes out right if every copy of every copy reaches the sink exactly once.
#[test]
fn two_stacked_streams_that_both_ask_again_each_get_their_turn() {
    let source = Arc::new(Counting::new((1..=10).collect(), 10, 10));
    let sink = Arc::new(Total::default());
    let built = pipeline(source, Arc::clone(&sink))
        .then(Arc::new(Repeating { copies: 2 }) as Arc<dyn DynStream>)
        .then(Arc::new(Repeating { copies: 3 }) as Arc<dyn DynStream>);

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 330);
}

#[test]
fn a_stream_that_says_done_stops_the_whole_pipeline() {
    let source = Arc::new(Counting::new((1..=100).collect(), 10, 10));
    let sink = Arc::new(Total::default());
    let built = pipeline(source, Arc::clone(&sink))
        .then(Arc::new(StopAfter { limit: 10 }) as Arc<dyn DynStream>);

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 55);
    assert_eq!(sink.combines.load(Ordering::Relaxed), 1);
    assert_eq!(sink.finalizes.load(Ordering::Relaxed), 1);
}

#[test]
fn an_empty_source_still_combines_and_finalizes() {
    let source = Arc::new(Counting::new(Vec::new(), 4, 4));
    let sink = Arc::new(Total::default());
    let built = pipeline(source, Arc::clone(&sink));

    run_serial(&built, &Cancel::new()).unwrap();

    assert_eq!(*sink.global.lock().unwrap(), 0);
    assert_eq!(sink.combines.load(Ordering::Relaxed), 1);
    assert_eq!(sink.finalizes.load(Ordering::Relaxed), 1);
}

#[test]
fn a_cancelled_query_stops_and_says_so() {
    let source = Arc::new(Counting::new((1..=1000).collect(), 10, 10));
    let sink = Arc::new(Total::default());
    let built = pipeline(source, Arc::clone(&sink));

    let cancel = Cancel::new();
    cancel.cancel();
    let error = run_serial(&built, &cancel).unwrap_err();

    assert_eq!(error.code(), ErrorCode::Interrupt);
}

#[test]
fn the_serial_driver_names_the_reason_it_cannot_park() {
    let sink = Arc::new(Total::default());
    let built = Pipeline::new(
        PipelineId(3),
        Arc::new(AlwaysBlocked) as Arc<dyn Source>,
        Arc::clone(&sink) as Arc<dyn DynSink>,
    );

    let error = run_serial(&built, &Cancel::new()).unwrap_err();

    assert_eq!(error.code(), ErrorCode::NotImplemented);
    assert!(error.message().contains("pipeline 3"), "{}", error.message());
    assert!(error.message().contains("waiting for read 7"), "{}", error.message());
}

#[test]
fn a_pipeline_records_what_it_waits_for() {
    let sink = Arc::new(Total::default());
    let first = Pipeline::new(
        PipelineId(0),
        Arc::new(Counting::new(vec![1], 1, 1)) as Arc<dyn Source>,
        Arc::clone(&sink) as Arc<dyn DynSink>,
    );
    let second = Pipeline::new(
        PipelineId(1),
        Arc::new(Counting::new(vec![1], 1, 1)) as Arc<dyn Source>,
        sink as Arc<dyn DynSink>,
    )
    .after(first.id());

    assert!(first.depends_on().is_empty());
    assert_eq!(second.depends_on(), &[PipelineId(0)]);
}

#[test]
fn the_root_hands_chunks_to_a_caller_outside_the_engine() {
    let (sink, reader) = root(BufferId(0), None);
    let source = Arc::new(Counting::new((1..=9).collect(), 3, 3));
    let built =
        Pipeline::new(PipelineId(0), source as Arc<dyn Source>, Arc::new(sink) as Arc<dyn DynSink>);

    assert!(!reader.is_finished());
    run_serial(&built, &Cancel::new()).unwrap();
    assert!(reader.is_finished());

    let chunks = reader.drain().unwrap();
    assert_eq!(chunks.len(), 3);
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    assert_eq!(rows, 9);
    assert!(reader.next_chunk().unwrap().is_none());
}

#[test]
fn a_full_root_queue_reports_backpressure_through_the_same_four_reasons() {
    let (sink, reader) = root(BufferId(4), Some(1));
    let source = Arc::new(Counting::new((1..=9).collect(), 3, 3));
    let built =
        Pipeline::new(PipelineId(0), source as Arc<dyn Source>, Arc::new(sink) as Arc<dyn DynSink>);

    let error = run_serial(&built, &Cancel::new()).unwrap_err();

    assert_eq!(error.code(), ErrorCode::NotImplemented);
    assert!(error.message().contains("buffer 4"), "{}", error.message());
    assert_eq!(reader.queued().unwrap(), 1);
}

/// One column of big integers, which is all the root needs to be told apart from another chunk.
fn numbers(values: &[i64]) -> Chunk {
    let values: Vec<Value> = values.iter().map(|value| Value::BigInt(*value)).collect();
    Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &values).unwrap()]).unwrap()
}

/// What came out, flattened, so that a test can say what order it wanted in one line.
fn rows(chunks: &[Chunk]) -> Vec<i64> {
    let mut out = Vec::new();
    for chunk in chunks {
        // row at a time: reading a handful of test rows back out, where a kernel would be more code
        // than the thing it tests
        for row in 0..chunk.len() {
            if let Value::BigInt(value) = chunk.value_at(row, 0) {
                out.push(value);
            }
        }
    }
    out
}

/// The parallel driver in miniature, because there is not one yet.
///
/// Two instances, morsels handed out in order and finished out of order, which is what happens the
/// moment the thread reading the second row group is quicker than the thread reading the first.
#[test]
fn an_ordered_root_puts_the_morsels_back_the_way_the_source_cut_them() {
    let (sink, reader) = root_in_order(BufferId(0), None);
    let sink: Arc<dyn DynSink> = Arc::new(sink);
    let mut first = sink.local_state();
    let mut second = sink.local_state();

    sink.at_state(&Morsel::new(0, 0, 2), &mut first).unwrap();
    sink.at_state(&Morsel::new(1, 2, 4), &mut second).unwrap();

    sink.sink_state(&numbers(&[30, 40]), &mut second).unwrap();
    sink.combine_state(second).unwrap();
    assert_eq!(reader.queued().unwrap(), 0, "morsel zero is still being read");

    sink.sink_state(&numbers(&[10]), &mut first).unwrap();
    sink.sink_state(&numbers(&[20]), &mut first).unwrap();
    sink.combine_state(first).unwrap();
    sink.finalize_state(&crate::Lease::alone()).unwrap();

    assert_eq!(rows(&reader.drain().unwrap()), vec![10, 20, 30, 40]);
}

/// Taking a morsel and registering it at the sink are separate calls. A worker may take morsel zero
/// and lose the CPU before it registers, while another worker finishes morsel one and starts two.
/// The root must keep one behind the unregistered zero even though zero is not in its local state.
#[test]
fn an_ordered_root_waits_for_a_morsel_that_was_taken_but_not_registered() {
    let (sink, reader) = root_in_order(BufferId(0), None);
    let sink: Arc<dyn DynSink> = Arc::new(sink);
    let mut later = sink.local_state();

    sink.at_state(&Morsel::new(1, 1, 2), &mut later).unwrap();
    sink.sink_state(&numbers(&[20]), &mut later).unwrap();
    sink.at_state(&Morsel::new(2, 2, 3), &mut later).unwrap();

    assert_eq!(reader.queued().unwrap(), 0, "morsel zero has been taken but not registered yet");

    let mut first = sink.local_state();
    sink.at_state(&Morsel::new(0, 0, 1), &mut first).unwrap();
    sink.sink_state(&numbers(&[10]), &mut first).unwrap();
    sink.combine_state(first).unwrap();
    sink.sink_state(&numbers(&[30]), &mut later).unwrap();
    sink.combine_state(later).unwrap();
    sink.finalize_state(&crate::Lease::alone()).unwrap();

    assert_eq!(rows(&reader.drain().unwrap()), vec![10, 20, 30]);
}

/// The same sequence against the plain root, which is the thing the ordered one exists to differ
/// from. A query with an `ORDER BY` under it wants this, because the operator below has already put
/// the rows where it wants them and holding chunks back would only add latency.
#[test]
fn a_plain_root_hands_chunks_on_in_the_order_they_arrive() {
    let (sink, reader) = root(BufferId(0), None);
    let sink: Arc<dyn DynSink> = Arc::new(sink);
    let mut first = sink.local_state();
    let mut second = sink.local_state();

    sink.at_state(&Morsel::new(0, 0, 2), &mut first).unwrap();
    sink.at_state(&Morsel::new(1, 2, 4), &mut second).unwrap();

    sink.sink_state(&numbers(&[30, 40]), &mut second).unwrap();
    sink.combine_state(second).unwrap();
    sink.sink_state(&numbers(&[10, 20]), &mut first).unwrap();
    sink.combine_state(first).unwrap();
    sink.finalize_state(&crate::Lease::alone()).unwrap();

    assert_eq!(rows(&reader.drain().unwrap()), vec![30, 40, 10, 20]);
}

/// Several chunks out of one morsel keep their order within it, which is the second half of the key
/// and the part that a morsel wider than a chunk depends on.
#[test]
fn chunks_from_one_morsel_keep_the_order_they_were_read_in() {
    let (sink, reader) = root_in_order(BufferId(0), None);
    let source = Arc::new(Counting::new((1..=12).collect(), 12, 3));
    let built =
        Pipeline::new(PipelineId(0), source as Arc<dyn Source>, Arc::new(sink) as Arc<dyn DynSink>);

    run_serial(&built, &Cancel::new()).unwrap();

    assert!(reader.is_finished());
    assert_eq!(rows(&reader.drain().unwrap()), (1..=12).collect::<Vec<i64>>());
}

/// A pipeline over an empty source takes no morsel at all, so nothing is ever released by taking
/// the next one and finalising has to be the thing that lets go.
#[test]
fn an_ordered_root_over_an_empty_source_finishes_with_nothing_held() {
    let (sink, reader) = root_in_order(BufferId(0), None);
    let source = Arc::new(Counting::new(Vec::new(), 4, 4));
    let built =
        Pipeline::new(PipelineId(0), source as Arc<dyn Source>, Arc::new(sink) as Arc<dyn DynSink>);

    run_serial(&built, &Cancel::new()).unwrap();

    assert!(reader.is_finished());
    assert!(reader.drain().unwrap().is_empty());
}

#[test]
fn an_ordered_root_refuses_a_driver_that_does_not_say_where_a_chunk_came_from() {
    let (sink, _reader) = root_in_order(BufferId(0), None);
    let mut place = sink.local();

    let error = sink.sink(&numbers(&[1]), &mut place).unwrap_err();

    assert_eq!(error.code(), ErrorCode::Internal);
    assert!(error.message().contains("which morsel"), "{}", error.message());
}

/// The bound on held chunks must never stop the instance whose chunks are next out, because there
/// is nobody left who could unblock it.
#[test]
fn the_earliest_morsel_is_never_told_to_wait_for_the_ones_behind_it() {
    let (sink, reader) = root_in_order(BufferId(4), Some(1));
    let sink: Arc<dyn DynSink> = Arc::new(sink);
    let mut first = sink.local_state();
    let mut second = sink.local_state();

    sink.at_state(&Morsel::new(0, 0, 4), &mut first).unwrap();
    sink.at_state(&Morsel::new(1, 4, 8), &mut second).unwrap();

    assert_eq!(sink.sink_state(&numbers(&[30]), &mut second).unwrap(), Progress::More);
    let blocked = sink.sink_state(&numbers(&[40]), &mut second).unwrap();
    assert_eq!(blocked, Progress::Blocked(Blocked::Downstream(BufferId(4))));
    assert_eq!(sink.sink_state(&numbers(&[10]), &mut first).unwrap(), Progress::More);
    assert_eq!(sink.sink_state(&numbers(&[20]), &mut first).unwrap(), Progress::More);

    sink.combine_state(first).unwrap();
    assert_eq!(rows(&reader.drain().unwrap()), vec![10, 20]);

    // Morsel one is the earliest being read now, so the chunk it was told to wait with goes through.
    assert_eq!(sink.sink_state(&numbers(&[40]), &mut second).unwrap(), Progress::More);
    sink.combine_state(second).unwrap();
    sink.finalize_state(&crate::Lease::alone()).unwrap();

    assert_eq!(rows(&reader.drain().unwrap()), vec![30, 40]);
}

#[test]
fn a_morsel_tracks_where_the_source_got_to() {
    let mut morsel = Morsel::new(2, 10, 20);
    assert_eq!(morsel.len(), 10);
    assert_eq!(morsel.remaining(), 10);
    assert!(!morsel.is_drained());

    morsel.advance(4);
    assert_eq!(morsel.cursor(), 14);
    assert_eq!(morsel.remaining(), 6);

    morsel.advance(1000);
    assert_eq!(morsel.cursor(), 20);
    assert!(morsel.is_drained());
    assert_eq!(morsel.remaining(), 0);
}

#[test]
fn an_empty_morsel_is_drained_from_the_start() {
    let morsel = Morsel::new(0, 5, 5);
    assert!(morsel.is_empty());
    assert!(morsel.is_drained());
}

#[test]
fn wrong_local_state_is_reported_rather_than_panicked_on() {
    let mut state = LocalState::new(7_usize);
    assert_eq!(*state.downcast_mut::<usize>().unwrap(), 7);

    let error = state.downcast_mut::<String>().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);

    let error = LocalState::new(7_usize).downcast::<String>().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
}

#[test]
fn an_operator_handed_another_operators_state_says_so() {
    let stream: Arc<dyn DynStream> = Arc::new(Evens);
    let sink: Arc<dyn DynSink> = Arc::new(Total::default());
    let mut wrong = sink.local_state();
    let mut chunk =
        Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1)]).unwrap()])
            .unwrap();

    let error = stream.push_state(&mut chunk, &mut wrong).unwrap_err();

    assert_eq!(error.code(), ErrorCode::Internal);
}

#[test]
fn every_blocked_reason_has_a_name_and_there_are_four_of_them() {
    assert_eq!(BlockedReason::ALL.len(), 4);
    for reason in BlockedReason::ALL {
        assert!(!reason.name().is_empty());
    }
    assert_eq!(Blocked::Io(IoToken(0)).reason(), BlockedReason::Io);
    assert_eq!(Blocked::Downstream(BufferId(0)).reason(), BlockedReason::Downstream);
}

#[test]
fn a_watched_pipeline_counts_the_rows_and_the_time_at_every_operator() {
    let scan = Arc::new(Counters::new(0, 0, "Counting"));
    let filter = Arc::new(Counters::new(1, 0, "Evens"));
    let total = Arc::new(Counters::new(2, 0, "Total"));
    let source = Arc::new(Watched::new(Counting::new((1..=10).collect(), 4, 4), Arc::clone(&scan)));
    let stream = Arc::new(Watched::new(Evens, Arc::clone(&filter)));
    let sink = Arc::new(Watched::new(Total::default(), Arc::clone(&total)));
    let built = Pipeline::new(PipelineId(0), source, sink as Arc<dyn DynSink>)
        .then(stream as Arc<dyn DynStream>);

    run_serial(&built, &Cancel::new()).expect("the pipeline runs");

    let read = scan.snapshot();
    let kept = filter.snapshot();
    let summed = total.snapshot();
    assert_eq!(read.rows_out, 10);
    assert_eq!(kept.rows_in, 10);
    assert_eq!(kept.rows_out, 5, "half of one to ten is even");
    assert_eq!(summed.rows_in, 5);
    assert!(read.wall_ns > 0, "reading ten rows took longer than nothing");
    assert!(kept.wall_ns > 0, "filtering them took longer than nothing");
}

/// A stream that gives up on the compact form of every chunk it is handed, and says so.
///
/// This is what the real ones look like from the shim's point of view: it does not know what a
/// kernel is or what a flatten is, it only knows that the count went up while this call was on the
/// stack.
#[derive(Debug)]
struct GivesUp;

impl Stream for GivesUp {
    type Local = ();

    fn local(&self) {}

    fn push(&self, _chunk: &mut Chunk, (): &mut ()) -> rudb_common::Result<Progress> {
        slow::took(Cause::Flatten);
        slow::took(Cause::Compare);
        Ok(Progress::More)
    }
}

#[test]
fn falling_back_is_charged_to_the_operator_that_did_it_and_to_no_other() {
    slow::reset();
    let scan = Arc::new(Counters::new(0, 0, "Counting"));
    let giving_up = Arc::new(Counters::new(1, 0, "GivesUp"));
    let total = Arc::new(Counters::new(2, 0, "Total"));
    let source = Arc::new(Watched::new(Counting::new((1..=10).collect(), 4, 4), Arc::clone(&scan)));
    let stream = Arc::new(Watched::new(GivesUp, Arc::clone(&giving_up)));
    let sink = Arc::new(Watched::new(Total::default(), Arc::clone(&total)));
    let built = Pipeline::new(PipelineId(0), source, sink as Arc<dyn DynSink>)
        .then(stream as Arc<dyn DynStream>);

    run_serial(&built, &Cancel::new()).expect("the pipeline runs");

    let gave_up = giving_up.snapshot();
    let chunks = gave_up.fallbacks.get(Cause::Flatten);
    assert!(chunks > 0, "the stream was called at least once");
    assert_eq!(gave_up.fallbacks.get(Cause::Compare), chunks, "both were counted every time");
    assert_eq!(gave_up.fallbacks.total(), chunks * 2);
    assert_eq!(gave_up.fallbacks.worst(), Some((Cause::Flatten, chunks)), "ties go to the first");
    assert!(scan.snapshot().fallbacks.is_empty(), "the source never gave up on anything");
    assert!(total.snapshot().fallbacks.is_empty(), "neither did the sink");
    slow::reset();
}

#[test]
fn a_call_that_produced_no_rows_is_still_measured() {
    let counters = Arc::new(Counters::new(0, 0, "AlwaysBlocked"));
    let source = Arc::new(Watched::new(AlwaysBlocked, Arc::clone(&counters)));
    let sink: Arc<dyn DynSink> = Arc::new(Total::default());
    let built = Pipeline::new(PipelineId(0), source, sink);

    let error = run_serial(&built, &Cancel::new()).expect_err("a blocked source has nowhere to go");

    assert_eq!(error.code(), ErrorCode::NotImplemented);
    let blocked = counters.snapshot();
    assert_eq!(blocked.rows_out, 0);
    assert_eq!(blocked.kind, "AlwaysBlocked");
}

/// A stream that fails on the first chunk it is given, whichever thread that is.
#[derive(Debug)]
struct Breaks;

impl Stream for Breaks {
    type Local = ();

    fn local(&self) {}

    fn push(&self, _chunk: &mut Chunk, (): &mut ()) -> rudb_common::Result<Progress> {
        Err(rudb_common::Error::internal("this operator always gives up"))
    }
}

/// A stream that panics on a thread that is not the caller's, which is the hang this pool could be.
#[derive(Debug)]
struct Panics {
    caller: std::thread::ThreadId,
}

impl Stream for Panics {
    type Local = ();

    /// Panics on a worker and not on the thread that asked, which is what makes this test decide
    /// something rather than race. Every instance builds its local state before it reads a row, so
    /// a worker that exists has been here, whereas a worker might never reach `push` if the calling
    /// thread got through all the morsels first.
    fn local(&self) {
        assert_eq!(std::thread::current().id(), self.caller, "a worker built a local state");
    }

    fn push(&self, _chunk: &mut Chunk, (): &mut ()) -> rudb_common::Result<Progress> {
        Ok(Progress::More)
    }
}

/// A stream that will not run as a second instance, which is what a `LIMIT` says.
#[derive(Debug)]
struct OnlyOnce;

impl Stream for OnlyOnce {
    type Local = ();

    fn local(&self) {}

    fn parallel(&self) -> bool {
        false
    }

    fn push(&self, _chunk: &mut Chunk, (): &mut ()) -> rudb_common::Result<Progress> {
        Ok(Progress::More)
    }
}

#[test]
fn a_pool_lends_what_it_has_and_not_more() {
    let pool = Pool::new(4);
    assert_eq!(pool.threads(), 4);
    let lease = pool.lease(10);
    assert_eq!(lease.degree(), 4, "asking for ten on a pool of four gets four");
    let second = pool.lease(4);
    assert_eq!(second.degree(), 1, "the first lease took them all, so this one is just its caller");
}

#[test]
fn a_lease_gives_its_threads_back_when_it_goes_away() {
    let pool = Pool::new(8);
    {
        let held = pool.lease(8);
        assert_eq!(held.degree(), 8);
    }
    assert_eq!(pool.lease(8).degree(), 8, "the pipeline that had them has finished");
}

#[test]
fn a_pool_of_one_lends_one_however_many_are_asked_for() {
    let pool = Pool::default();
    assert_eq!(pool.lease(32).degree(), 1);
}

#[test]
fn a_lease_of_nobody_runs_the_caller_and_starts_nothing() {
    let alone = crate::Lease::alone();
    assert_eq!(alone.degree(), 1, "a caller with no pool to ask is still a thread");
    let ran = AtomicUsize::new(0);
    let (mine, panicked) = alone.scatter(
        &|| {
            ran.fetch_add(1, Ordering::Relaxed);
        },
        || 7,
    );
    assert_eq!(mine, 7, "the caller's own work is what comes back");
    assert_eq!(ran.load(Ordering::Relaxed), 0, "and nothing else ran it");
    assert!(!panicked);
}

#[test]
fn a_scatter_takes_no_more_threads_than_the_work_has_pieces() {
    let pool = Pool::new(8);
    let lease = pool.lease(8);
    assert_eq!(lease.degree(), 8);
    let ran = AtomicUsize::new(0);
    let count = || {
        ran.fetch_add(1, Ordering::Relaxed);
    };
    lease.scatter_at_most(3, &count, count);
    assert_eq!(ran.load(Ordering::Relaxed), 3, "two borrowed threads and the caller, not eight");

    ran.store(0, Ordering::Relaxed);
    lease.scatter_at_most(1, &count, count);
    assert_eq!(ran.load(Ordering::Relaxed), 1, "one piece is the caller and nobody woken");

    ran.store(0, Ordering::Relaxed);
    lease.scatter_at_most(99, &count, count);
    assert_eq!(ran.load(Ordering::Relaxed), 8, "asking for more than the lease gets the lease");
}

#[test]
fn the_degree_of_a_pipeline_is_the_smaller_of_the_ceiling_and_the_work() {
    let source = Arc::new(Counting::new((1..=100).collect(), 10, 4));
    let sink = Arc::new(Total::default());
    let built = pipeline(source as Arc<dyn Source>, sink);

    assert_eq!(built.degree(4), 4, "ten morsels is more than enough for four threads");
    assert_eq!(built.degree(32), 10, "and not enough for thirty two");
    assert_eq!(built.degree(1), 1);
}

#[test]
fn a_source_is_told_how_many_workers_are_coming_even_when_the_answer_is_one() {
    // Asking is how a source is told, not only how it is counted, so the one worker case has to
    // reach it. A scan cuts a morsel per stripe when it is asked and a morsel per part when it is
    // not, and the second kind is one it cannot walk ruled out parts inside of, which made a
    // single threaded point lookup pay for every part it had already proved held nothing.
    let source = Arc::new(Asked::default());
    let built = pipeline(Arc::clone(&source) as Arc<dyn Source>, Arc::new(Total::default()));

    assert_eq!(built.degree(1), 1, "one worker, however much work the source says it has");
    assert_eq!(source.calls.load(Ordering::Relaxed), 1, "and it was asked rather than assumed");
    assert_eq!(
        source.told.load(Ordering::Relaxed),
        1,
        "and told the one rather than something else"
    );
}

#[test]
fn a_source_under_an_operator_that_refuses_instances_is_told_one_rather_than_the_ceiling() {
    // The pipeline is going to run on one thread whatever the pool would lend, so telling the
    // source the ceiling would have it cut for workers that are never coming.
    let source = Arc::new(Asked::default());
    let built = Pipeline::new(
        PipelineId(0),
        Arc::clone(&source) as Arc<dyn Source>,
        Arc::new(Total::default()) as Arc<dyn DynSink>,
    )
    .then(Arc::new(OnlyOnce) as Arc<dyn DynStream>);

    assert!(!built.parallel());
    assert_eq!(built.degree(32), 1);
    assert_eq!(source.told.load(Ordering::Relaxed), 1);
}

#[test]
fn one_operator_that_refuses_a_second_instance_keeps_the_whole_pipeline_on_one_thread() {
    let source = Arc::new(Counting::new((1..=100).collect(), 10, 4));
    let sink = Arc::new(Total::default());
    let built = Pipeline::new(
        PipelineId(0),
        source as Arc<dyn Source>,
        Arc::clone(&sink) as Arc<dyn DynSink>,
    )
    .then(Arc::new(OnlyOnce) as Arc<dyn DynStream>);

    assert!(!built.parallel());
    assert_eq!(built.degree(32), 1);
}

#[test]
fn the_parallel_driver_answers_what_the_serial_one_answers() {
    let values: Vec<i64> = (1..=10_000).collect();
    let expected: i64 = values.iter().sum();

    let sink = Arc::new(Total::default());
    let built = pipeline(
        Arc::new(Counting::new(values.clone(), 100, 32)) as Arc<dyn Source>,
        Arc::clone(&sink),
    );
    run_parallel(&built, &Cancel::new(), &Pool::new(8).lease(8)).expect("it runs");

    assert_eq!(*sink.global.lock().unwrap(), expected);
    assert_eq!(sink.combines.load(Ordering::Relaxed), 8, "one combine per instance");
    assert_eq!(sink.finalizes.load(Ordering::Relaxed), 1, "and one finalize for all of them");
}

#[test]
fn every_morsel_is_read_once_however_many_threads_read_them() {
    let source = Arc::new(Counting::new((1..=10_000).collect(), 100, 100));
    let sink = Arc::new(Total::default());
    let built = pipeline(Arc::clone(&source) as Arc<dyn Source>, Arc::clone(&sink));

    run_parallel(&built, &Cancel::new(), &Pool::new(8).lease(8)).expect("it runs");

    assert_eq!(source.reads.load(Ordering::Relaxed), 100, "a hundred morsels of one chunk each");
}

#[test]
fn a_degree_of_one_is_the_serial_driver() {
    let sink = Arc::new(Total::default());
    let built = pipeline(
        Arc::new(Counting::new((1..=10).collect(), 4, 4)) as Arc<dyn Source>,
        Arc::clone(&sink),
    );

    let spent = run_parallel(&built, &Cancel::new(), &Pool::new(1).lease(1)).expect("it runs");

    assert_eq!(*sink.global.lock().unwrap(), 55);
    assert_eq!(sink.combines.load(Ordering::Relaxed), 1);
    assert_eq!(spent.worker_cpu_ns, 0, "nothing ran anywhere the caller's own clock could not see");
}

#[test]
fn the_parallel_driver_reports_what_its_workers_burned() {
    let sink = Arc::new(Total::default());
    let built = pipeline(
        Arc::new(Counting::new((1..=200_000).collect(), 1_000, 1_000)) as Arc<dyn Source>,
        Arc::clone(&sink),
    );

    let spent = run_parallel(&built, &Cancel::new(), &Pool::new(4).lease(4)).expect("it runs");

    if thread_cpu_ns().is_some() {
        assert!(
            spent.worker_cpu_ns > 0,
            "three of the four threads were not the caller's and they did something"
        );
        assert!(
            spent.slowest_ns > 0,
            "one of the four instances took the longest and it was not instant"
        );
    }
}

#[test]
fn an_instance_that_fails_stops_the_others_and_the_query_says_why() {
    let sink = Arc::new(Total::default());
    let built = Pipeline::new(
        PipelineId(0),
        Arc::new(Counting::new((1..=10_000).collect(), 10, 10)) as Arc<dyn Source>,
        Arc::clone(&sink) as Arc<dyn DynSink>,
    )
    .then(Arc::new(Breaks) as Arc<dyn DynStream>);

    let error = run_parallel(&built, &Cancel::new(), &Pool::new(4).lease(4)).unwrap_err();

    assert_eq!(error.message(), "this operator always gives up");
    assert_eq!(sink.finalizes.load(Ordering::Relaxed), 0, "a failed pipeline has no answer");
}

#[test]
fn the_pool_starts_its_workers_once_and_keeps_them_for_the_next_query() {
    let pool = Pool::new(4);
    assert_eq!(pool.workers(), 0, "a pool that has run nothing has started nothing");

    for _ in 0..3 {
        let sink = Arc::new(Total::default());
        let built = pipeline(
            Arc::new(Counting::new((1..=10_000).collect(), 100, 32)) as Arc<dyn Source>,
            Arc::clone(&sink),
        );
        run_parallel(&built, &Cancel::new(), &pool.lease(4)).expect("it runs");
        assert_eq!(*sink.global.lock().unwrap(), 50_005_000);
    }

    assert_eq!(pool.workers(), 3, "three runs of four threads borrowed the same three workers");
}

#[test]
fn a_worker_that_panics_fails_the_query_rather_than_leaving_it_waiting() {
    // The panic is printed by the thread it happens on, and this test means to cause one, so the
    // hook is taken off for the duration rather than letting it write a backtrace to a passing run.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let sink = Arc::new(Total::default());
    let built = Pipeline::new(
        PipelineId(0),
        Arc::new(Counting::new((1..=10_000).collect(), 10, 10)) as Arc<dyn Source>,
        Arc::clone(&sink) as Arc<dyn DynSink>,
    )
    .then(Arc::new(Panics { caller: std::thread::current().id() }) as Arc<dyn DynStream>);

    let error = run_parallel(&built, &Cancel::new(), &Pool::new(4).lease(4)).unwrap_err();

    std::panic::set_hook(hook);
    assert_eq!(error.message(), "a thread running part of this query panicked");
    assert_eq!(sink.finalizes.load(Ordering::Relaxed), 0, "a failed pipeline has no answer");
}
