//! Build an explicit ordered, covering projection in a native file.

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let result = match args.as_slice() {
        [_, path, table, order, covered] => {
            rudb_native::build_sorted_projection(path, table, order, covered)
        }
        [_, flag, path, table, order, covered] if flag == "--cluster-covered" => {
            rudb_native::build_clustered_projection(path, table, order, covered)
        }
        _ => {
            eprintln!(
                "usage: rudb-projection [--cluster-covered] FILE TABLE ORDER_COLUMN COVERED_COLUMN"
            );
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
