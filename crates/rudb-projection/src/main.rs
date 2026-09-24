//! Build an explicit ordered, covering projection in a native file.

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let [_, path, table, order, covered] = args.as_slice() else {
        eprintln!("usage: rudb-projection FILE TABLE ORDER_COLUMN COVERED_COLUMN");
        std::process::exit(2);
    };
    if let Err(error) = rudb_native::build_sorted_projection(path, table, order, covered) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
