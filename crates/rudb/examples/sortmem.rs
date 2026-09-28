fn peak() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc_rusage>::zeroed();
    unsafe { getrusage(0, usage.as_mut_ptr()) };
    unsafe { usage.assume_init() }.maxrss as u64
}
#[repr(C)]
struct libc_rusage {
    utime: [i64; 2],
    stime: [i64; 2],
    maxrss: i64,
    rest: [i64; 13],
}
unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut libc_rusage) -> i32;
}
fn main() {
    let budget: u64 = std::env::args().nth(1).map_or(128, |a| a.parse().unwrap());
    let db = rudb::Database::with_config(
        rudb::Config::default()
            .with_memory_limit(budget << 20)
            .with_threads(std::env::args().nth(3).map_or(0, |t| t.parse().unwrap()))
            .unwrap(),
    );
    let sql = std::env::args().nth(2).unwrap();
    let r = db.execute(&sql);
    println!(
        "{:?} peak_mb={}",
        r.map(|r| format!("{r:?}").len()).map_err(|e| e.to_string()),
        peak() >> 20
    );
}
