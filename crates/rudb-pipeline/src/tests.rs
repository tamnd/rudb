//! The contract of the three traits, exercised with the smallest operators that have one.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{Cancel, Cause, ErrorCode, LogicalType, Value, slow};
use rudb_metrics::Counters;
use rudb_vector::{Chunk, Vector};

use crate::dynamic::{DynSink, DynStream, LocalState};
use crate::morsel::Morsel;
use crate::pipeline::Pipeline;
use crate::progress::{Blocked, BlockedReason, BufferId, IoToken, PipelineId, Progress};
use crate::root::root;
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

    fn finalize(&self) -> rudb_common::Result<()> {
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

fn pipeline(source: Arc<dyn Source>, sink: Arc<Total>) -> Pipeline {
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
