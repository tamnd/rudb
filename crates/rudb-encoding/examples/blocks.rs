//! Throwaway: time the payload shapes over dictionary blocks read from a file of lines.
use rudb_encoding::chooser::Settled;
use rudb_encoding::{integer, string};

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let only = std::env::args().nth(2);
    let text = std::fs::read(&path).unwrap();
    let values: Vec<&[u8]> = text.split(|b| *b == b'\n').filter(|v| !v.is_empty()).collect();
    let blocks: Vec<Vec<&[u8]>> = values.chunks(1024).map(<[_]>::to_vec).collect();
    let bytes: usize = values.iter().map(|v| v.len()).sum();
    let shapes = [
        vec![string::Kind::Front, string::Kind::Lz],
        vec![string::Kind::Front, string::Kind::Lz, string::Kind::Plain],
        vec![string::Kind::Front, string::Kind::Lz, string::Kind::Fsst],
        vec![string::Kind::Front, string::Kind::Plain],
        vec![string::Kind::Front, string::Kind::Fsst],
        vec![string::Kind::Lz, string::Kind::Fsst],
        vec![string::Kind::Lz, string::Kind::Plain],
        vec![string::Kind::Fsst],
        vec![string::Kind::Plain],
    ];
    let step = (blocks.len() / 8).max(1);
    let sample: Vec<Vec<&[u8]>> = blocks.iter().step_by(step).take(8).cloned().collect();
    for strings in shapes {
        let name = format!("{strings:?}");
        if only.as_ref().is_some_and(|o| !name.contains(o.as_str())) {
            continue;
        }
        let shape = string::with_symbols(Settled::new(strings, vec![integer::Kind::Packed]), &sample);
        let started = std::time::Instant::now();
        let mut out = 0;
        for _ in 0..std::env::var("REPEAT").ok().and_then(|v| v.parse().ok()).unwrap_or(1) { for block in &blocks {
            out += string::encode_with(block, &shape).unwrap().len(); }
        }
        let spent = started.elapsed();
        println!(
            "{name:32} {:>8.1} MB/s  ratio {:.2}  {:?}",
            bytes as f64 / spent.as_secs_f64() / 1e6,
            bytes as f64 / out as f64,
            spent
        );
    }
}
