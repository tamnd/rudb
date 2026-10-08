//! Scratch timing of the link path a reduced scan takes, not part of the suite.
use rudb_graph::{Link, Rids};
use std::time::Instant;

#[test]
fn zz_bench_link_path() {
    let parents = 200_000_u64;
    let per = 4_u64;
    let parents_of: Vec<u64> = (0..parents * per).map(|c| c / per).collect();
    let link = Link::build(&parents_of, parents).unwrap();
    let mut held = Vec::new();
    let mut x = 12345_u64;
    for p in 0..parents {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        if (x >> 33) % 268 == 0 {
            held.push(p);
        }
    }
    let set = Rids::from_sorted(parents, held.clone()).unwrap();
    let reps = 300;
    let t = Instant::now();
    let mut pushed = None;
    for _ in 0..reps {
        pushed = Some(std::hint::black_box(set.forward_or_stop(&link).unwrap()));
    }
    let push_ns = t.elapsed().as_nanos() / reps;
    let rows = pushed.unwrap().rids;
    let children = parents * per;
    let part = 8192_u64;
    let t = Instant::now();
    let mut total = 0;
    let mut all = Vec::new();
    for _ in 0..reps {
        all.clear();
        for first in (0..children).step_by(part as usize) {
            let len = part.min(children - first) as usize;
            let offsets = std::hint::black_box(rows.offsets_in(first, len));
            total += offsets.len();
            all.push((first, offsets));
        }
    }
    let offsets_ns = t.elapsed().as_nanos() / reps;
    let kept = total / reps as usize;
    let t = Instant::now();
    let mut out = Vec::new();
    let mut sum = 0_u64;
    for _ in 0..reps {
        for (first, offsets) in &all {
            let kids: Vec<u64> = offsets.iter().map(|&o| first + u64::from(o)).collect();
            link.forward_each(&kids, &mut out);
            sum = sum.wrapping_add(out.iter().sum::<u64>());
        }
    }
    let each_ns = t.elapsed().as_nanos() / reps;
    std::hint::black_box(sum);
    println!(
        "BENCH held {} kept {kept} push {push_ns} ns, offsets_in {offsets_ns} ns, forward_each {each_ns} ns ({} ns a child)",
        held.len(),
        each_ns / kept as u128
    );
}
