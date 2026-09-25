#![allow(unsafe_code)]

use rudb_common::Cancel;
use rudb_plan::Plan;
use rudb_qc_interp::Program;
use rudb_qc_rt::abi::{Col, Morsel};

use super::*;

fn graph(text: &str) -> Graph {
    let plan = Plan::parse(text).expect("the test plan parses");
    rudb_qc_pipe::split(&rudb_qc_plan::lower(&plan).expect("the test plan lowers"))
}

/// A source column as the driver hands it over: values at their width and a validity bitmap.
#[derive(Clone)]
struct Column {
    values: Vec<u8>,
    valid: Vec<u8>,
}

fn ints(values: &[Option<i32>]) -> Column {
    let mut c = Column { values: Vec::new(), valid: vec![0; values.len().div_ceil(8)] };
    for (i, v) in values.iter().enumerate() {
        c.values.extend_from_slice(&v.unwrap_or(0).to_le_bytes());
        if v.is_some() {
            c.valid[i / 8] |= 1 << (i % 8);
        }
    }
    c
}

fn strings(values: &[&'static str]) -> Column {
    let mut c = Column { values: Vec::new(), valid: vec![0xff; values.len().div_ceil(8)] };
    for v in values {
        c.values.extend_from_slice(&text::make(v.as_bytes()).to_le_bytes());
    }
    c
}

/// Runs the body of `stage` over the eight rows of `columns` in one morsel, with `state` set up
/// by the caller.
fn run(query: &Query, stage: usize, rt: &mut Rt, columns: &[Column], state: &mut [u8]) {
    let status = call(query, stage, rt, columns, state);
    assert_eq!(status, 0, "{:?}", rt.take_error());
}

/// Like [`run`], and returns the status rather than wanting it to be `Ok`.
fn call(query: &Query, stage: usize, rt: &mut Rt, columns: &[Column], state: &mut [u8]) -> u64 {
    let body = query.bodies[stage].as_ref().expect("a pipeline");
    let cols: Vec<Col> = body
        .reads
        .iter()
        .map(|&c| Col { values: columns[c].values.as_ptr(), valid: columns[c].valid.as_ptr() })
        .collect();
    let rows = 8;
    let morsel = Morsel {
        source: 0,
        chunk: 0,
        begin: 0,
        end: rows as u32,
        seq: 0,
        enc: 0,
        flags: 0,
        cols: cols.as_ptr(),
    };
    let program = Program::new(&query.module);
    let f = program.func(&body.func).expect("the function");
    program.call(f, state.as_mut_ptr(), (&raw const morsel).cast(), rt)
}

#[test]
fn a_filter_and_a_projection_write_the_rows_that_pass() {
    let g = graph(concat!(
        "Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS y]\n",
        "  Filter (#0.0::INTEGER > 3::INTEGER)::BOOLEAN\n",
        "    Get memory.main.a AS a #0 [x::INTEGER]\n",
    ));
    let mut rt = Rt::new(Cancel::new());
    let query = generate(&g, &mut rt).expect("generates");
    let Some(Body { sink: Out::Result { count, columns, .. }, state, .. }) = &query.bodies[0]
    else {
        panic!("{:?}", query.bodies)
    };
    let x = ints(&[Some(1), Some(5), None, Some(7), Some(2), Some(9), Some(0), Some(4)]);
    let mut out = vec![0u8; 8 * 4];
    let mut valid = vec![0u8; 8];
    let mut st = vec![0u8; *state as usize];
    st[columns[0].values as usize..][..8].copy_from_slice(&(out.as_mut_ptr() as u64).to_le_bytes());
    st[columns[0].valid as usize..][..8]
        .copy_from_slice(&(valid.as_mut_ptr() as u64).to_le_bytes());
    run(&query, 0, &mut rt, &[x], &mut st);
    let n = u64::from_le_bytes(st[*count as usize..][..8].try_into().unwrap());
    let got: Vec<i32> = (0..n as usize)
        .map(|i| i32::from_le_bytes(out[4 * i..4 * i + 4].try_into().unwrap()))
        .collect();
    assert_eq!(got, [6, 8, 10, 5]);
    assert!(valid[..n as usize].iter().all(|v| *v == 1));
}

#[test]
fn a_grouped_count_over_strings_makes_one_group_per_value() {
    let g = graph(concat!(
        "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT, min(#0.1::INTEGER)::INTEGER]\n",
        "  Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
        "    Get memory.main.hits AS hits #0 [s::VARCHAR, n::INTEGER]\n",
    ));
    let mut rt = Rt::new(Cancel::new());
    let query = generate(&g, &mut rt).expect("generates");
    let Some(Body { sink: Out::Aggregate(grouping), state, .. }) = &query.bodies[0] else {
        panic!("{:?}", query.bodies)
    };
    let long = "a string that is longer than twelve bytes";
    let s = strings(&["a", "", "bb", "a", long, long, "a", ""]);
    let n = ints(&[Some(5), Some(1), Some(2), Some(3), None, Some(8), Some(4), Some(0)]);
    let mut st = vec![0u8; *state as usize];
    run(&query, 0, &mut rt, &[s, n], &mut st);
    let table = rt.table(grouping.table).expect("the table");
    assert_eq!(table.len(), 3);
    let mut groups = Vec::new();
    for gid in 0..table.len() {
        let row = table.row(gid);
        let key = u128::from_le_bytes(row[8..24].try_into().unwrap());
        // SAFETY: the table copied long keys into the runtime heap, which `rt` keeps.
        let key = String::from_utf8(unsafe { text::bytes(&key) }.to_vec()).unwrap();
        let acc = &row[grouping.acc_offset as usize..];
        let count = i64::from_le_bytes(acc[..8].try_into().unwrap());
        let min = i32::from_le_bytes(acc[8..12].try_into().unwrap());
        groups.push((key, count, min));
    }
    groups.sort();
    assert_eq!(groups, [("a".to_owned(), 3, 3), (long.to_owned(), 2, 8), ("bb".to_owned(), 1, 2)]);
}

#[test]
fn an_ungrouped_aggregate_updates_the_one_row() {
    let g = graph(concat!(
        "Aggregate #1 groups=[] aggregates=[sum(#0.0::INTEGER)::HUGEINT, count(#0.0::INTEGER)::BIGINT, max(#0.0::INTEGER)::INTEGER]\n",
        "  Get memory.main.a AS a #0 [x::INTEGER]\n",
    ));
    let mut rt = Rt::new(Cancel::new());
    let query = generate(&g, &mut rt).expect("generates");
    let Some(Body { sink: Out::Aggregate(grouping), state, .. }) = &query.bodies[0] else {
        panic!("{:?}", query.bodies)
    };
    let mut st = vec![0u8; *state as usize];
    let row = rt.table(grouping.table).expect("the table").address(0);
    let at = grouping.row.expect("a row slot") as usize;
    st[at..at + 8].copy_from_slice(&(row as u64).to_le_bytes());
    let x = ints(&[Some(-4), None, Some(10), Some(3), None, None, None, None]);
    run(&query, 0, &mut rt, &[x], &mut st);
    let row = rt.table(grouping.table).expect("the table").row(0);
    let acc = &row[grouping.acc_offset as usize..];
    let sum = i128::from_le_bytes(acc[..16].try_into().unwrap());
    let count = i64::from_le_bytes(acc[24..32].try_into().unwrap());
    let max = i32::from_le_bytes(acc[32..36].try_into().unwrap());
    assert_eq!((sum, acc[16], count, max, acc[36]), (9, 1, 3, 10, 1));
}

#[test]
fn a_function_with_no_translator_runs_the_first_engines_kernel_through_a_vcall() {
    let g = graph(concat!(
        "Project #1 [replace(#0.0::VARCHAR, 'a'::VARCHAR, 'xyz'::VARCHAR)::VARCHAR AS r]\n",
        "  Get memory.main.a AS a #0 [s::VARCHAR]\n",
    ));
    let mut rt = Rt::new(Cancel::new());
    let query = generate(&g, &mut rt).expect("generates");
    assert_eq!(query.module.kernels.len(), 1);
    let Some(Body { sink: Out::Result { count, columns, .. }, state, .. }) = &query.bodies[0]
    else {
        panic!("{:?}", query.bodies)
    };
    let long = "a string that is longer than twelve bytes";
    let s = strings(&["a", "", "bb", long, "a", "", "bb", long]);
    let mut out = vec![0u128; 8];
    let mut valid = vec![0u8; 8];
    let mut st = vec![0u8; *state as usize];
    st[columns[0].values as usize..][..8].copy_from_slice(&(out.as_mut_ptr() as u64).to_le_bytes());
    st[columns[0].valid as usize..][..8]
        .copy_from_slice(&(valid.as_mut_ptr() as u64).to_le_bytes());
    run(&query, 0, &mut rt, &[s], &mut st);
    let n = u64::from_le_bytes(st[*count as usize..][..8].try_into().unwrap());
    assert_eq!(n, 8);
    let got: Vec<String> = out
        .iter()
        // SAFETY: an answer longer than twelve bytes is in the kernel's heap, which `rt` keeps.
        .map(|h| String::from_utf8(unsafe { text::bytes(h) }.to_vec()).unwrap())
        .collect();
    let replaced = "xyz string thxyzt is longer thxyzn twelve bytes";
    assert_eq!(got, ["xyz", "", "bb", replaced, "xyz", "", "bb", replaced]);
    assert!(valid.iter().all(|v| *v == 1));
}

#[test]
fn a_function_with_no_arguments_is_refused_by_name() {
    let g = graph(concat!(
        "Project #1 [random()::DOUBLE AS r]\n",
        "  Get memory.main.a AS a #0 [s::VARCHAR]\n",
    ));
    let mut rt = Rt::new(Cancel::new());
    let refusal = generate(&g, &mut rt).expect_err("refused");
    assert_eq!(refusal.what, "random()");
}

fn put(st: &mut [u8], at: u32, word: u64) {
    st[at as usize..][..8].copy_from_slice(&word.to_le_bytes());
}

#[test]
fn a_join_builds_its_table_and_probes_it_once_per_match() {
    let g = graph(concat!(
        "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
        "  Get memory.main.l AS l #0 [a::INTEGER, x::INTEGER]\n",
        "  Get memory.main.r AS r #1 [a::INTEGER, s::VARCHAR]\n",
    ));
    let mut rt = Rt::new(Cancel::new());
    let query = generate(&g, &mut rt).expect("generates");

    // The build reads the right side, drops the null key and keeps both duplicates of 2.
    let Some(Body { sink: Out::Build(b), state, .. }) = &query.bodies[0] else {
        panic!("{:?}", query.bodies)
    };
    let long = "a string that is longer than twelve bytes";
    let ra = ints(&[Some(1), Some(2), Some(2), None, Some(5), Some(6), Some(7), Some(8)]);
    let rs = strings(&["one", "two", "deux", long, long, "six", "seven", "eight"]);
    let mut st = vec![0u8; *state as usize];
    run(&query, 0, &mut rt, &[ra, rs], &mut st);
    drop(st);
    let published = rt.finish_join(b.table).expect("the table finishes");
    assert_eq!(published.rows, 7);

    let Some(Body { sink: Out::Result { count, columns, capacity }, state, probes, .. }) =
        &query.bodies[1]
    else {
        panic!("{:?}", query.bodies)
    };
    let capacity = capacity.expect("a probe pipeline has a capacity");
    let la = ints(&[Some(2), None, Some(3), Some(1), Some(2), Some(9), Some(5), Some(5)]);
    let lx = ints(&[Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(7)]);
    let run_with = |rt: &mut Rt, room: u64| {
        let mut out: Vec<Vec<u128>> = vec![vec![0; room as usize]; 4];
        let mut valid: Vec<Vec<u8>> = vec![vec![0; room as usize]; 4];
        let mut st = vec![0u8; *state as usize];
        for (k, slot) in columns.iter().enumerate() {
            put(&mut st, slot.values, out[k].as_mut_ptr() as u64);
            put(&mut st, slot.valid, valid[k].as_mut_ptr() as u64);
        }
        put(&mut st, capacity, room);
        put(&mut st, probes[0].directory, published.directory as u64);
        put(&mut st, probes[0].shift, published.shift);
        put(&mut st, probes[0].tags, published.tags as u64);
        let status = call(&query, 1, rt, &[la.clone(), lx.clone()], &mut st);
        let n = u64::from_le_bytes(st[*count as usize..][..8].try_into().unwrap());
        (status, n, out)
    };
    let (status, _, _) = run_with(&mut rt, 4);
    assert_eq!(status, NEED_MEMORY, "seven matches do not fit in four rows");
    let (status, n, out) = run_with(&mut rt, 16);
    assert_eq!((status, n), (0, 7));
    let mut rows = Vec::new();
    for i in 0..n as usize {
        let word = |k: usize, w: usize| {
            // SAFETY: the buffers are u128s, read here as the bytes the body wrote.
            let bytes = unsafe {
                std::slice::from_raw_parts(out[k].as_ptr().cast::<u8>(), out[k].len() * 16)
            };
            i32::from_le_bytes(bytes[i * w..i * w + 4].try_into().unwrap())
        };
        let s = out[3][i];
        // SAFETY: the long string was copied into the runtime heap when it went into the table.
        let s = String::from_utf8(unsafe { text::bytes(&s) }.to_vec()).unwrap();
        rows.push((word(0, 4), word(1, 4), word(2, 4), s));
    }
    rows.sort();
    let want = [
        (1, 3, 1, "one"),
        (2, 0, 2, "deux"),
        (2, 0, 2, "two"),
        (2, 4, 2, "deux"),
        (2, 4, 2, "two"),
        (5, 6, 5, long),
        (5, 7, 5, long),
    ];
    let want: Vec<_> = want.iter().map(|(a, b, c, d)| (*a, *b, *c, (*d).to_owned())).collect();
    assert_eq!(rows, want);
}
