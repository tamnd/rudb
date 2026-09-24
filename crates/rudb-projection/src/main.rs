//! Build an explicit ordered, covering projection in a native file.

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let result = match args.as_slice() {
        [_, path, table, order, covered] => {
            rudb_native::build_sorted_projection(path, table, order, covered)
        }
        [_, flag, path, table, order, covered] if flag == "--run-length" => {
            rudb_native::build_run_projection(path, table, order, covered)
        }
        _ => {
            eprintln!(
                "usage: rudb-projection [--run-length] FILE TABLE ORDER_COLUMN COVERED_COLUMN"
            );
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
