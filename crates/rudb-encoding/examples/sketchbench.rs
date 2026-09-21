use rudb_encoding::sketch::{DEFAULT_K, Sketch, hash64};
use std::time::Instant;

fn main() {
    let n: u64 = 50_000_000;
    // Hashing alone, sixteen bytes at a time, the way count.rs hashes an integer.
    let started = Instant::now();
    let mut sink = 0u64;
    for i in 0..n {
        sink ^= hash64(&(i as i128).to_le_bytes());
    }
    let hashing = started.elapsed();
    println!(
        "hash only: {:?} ({:.1} ns/value) {sink}",
        hashing,
        hashing.as_nanos() as f64 / n as f64
    );

    for distinct in [10u64, 1000, 100_000, 50_000_000] {
        let mut sketch = Sketch::new(DEFAULT_K).unwrap();
        let started = Instant::now();
        for i in 0..n {
            sketch.add_hash(hash64(&((i % distinct) as i128).to_le_bytes()));
        }
        let whole = started.elapsed();
        println!(
            "{distinct} distinct: {:?} ({:.1} ns/value), sketch alone {:.1} ns/value",
            whole,
            whole.as_nanos() as f64 / n as f64,
            (whole.as_nanos() as f64 - hashing.as_nanos() as f64) / n as f64,
        );
    }
}
