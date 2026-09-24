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
    let status = program.call(f, state.as_mut_ptr(), (&raw const morsel).cast(), rt);
    assert_eq!(status, 0, "{:?}", rt.take_error());
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
    let Some(Body { sink: Out::Result { count, columns }, state, .. }) = &query.bodies[0] else {
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
fn a_function_it_does_not_know_is_refused_by_name() {
    let g = graph(concat!(
        "Project #1 [md5(#0.0::VARCHAR)::VARCHAR AS h]\n",
        "  Get memory.main.a AS a #0 [s::VARCHAR]\n",
    ));
    let mut rt = Rt::new(Cancel::new());
    let refusal = generate(&g, &mut rt).expect_err("refused");
    assert_eq!(refusal.what, "md5(VARCHAR)");
}
