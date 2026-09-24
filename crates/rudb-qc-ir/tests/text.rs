//! The text form round-trips, and the verifier accepts the modules it should and rejects the ones
//! under `tests/illegal/` for the rule each file is named after.

use std::fs;
use std::path::Path;

use rudb_qc_ir::{parse, print, verify};

fn files(dir: &str) -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(dir);
    let mut out: Vec<(String, String)> = fs::read_dir(&dir)
        .expect("the test directory exists")
        .map(|e| e.expect("a directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "qir"))
        .map(|p| {
            let name = p.file_stem().expect("a file name").to_string_lossy().into_owned();
            (name, fs::read_to_string(&p).expect("a readable file"))
        })
        .collect();
    out.sort();
    out
}

#[test]
fn legal_modules_verify_and_round_trip() {
    for (name, text) in files("text") {
        let m = parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Err(errors) = verify(&m) {
            let list: Vec<String> = errors.iter().map(ToString::to_string).collect();
            panic!("{name} does not verify:\n{}", list.join("\n"));
        }
        let once = print(&m);
        let again = parse(&once).unwrap_or_else(|e| panic!("{name} does not reparse: {e}\n{once}"));
        assert_eq!(print(&again), once, "{name} does not round-trip");
        assert_eq!(again.funcs[0].count(), m.funcs[0].count());
    }
}

#[test]
fn illegal_modules_are_rejected_for_their_rule() {
    let all = files("illegal");
    assert!(all.len() >= 4, "V9 to V12 each need a module");
    for (name, text) in all {
        let m = parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let rule = name.to_uppercase();
        let errors = verify(&m).expect_err(&format!("{name} verifies and should not"));
        assert!(
            errors.iter().all(|e| e.rule == rule),
            "{name} breaks other rules too: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }
}

#[test]
fn parse_errors_name_the_line() {
    let e = parse(
        "module x\n\nfunc @f version=v plan=#0\nblock b0(ptr %st, ptr %m):\n  %x = frob i64 1\n",
    )
    .unwrap_err();
    assert_eq!(e.line, 5);
    let e = parse("module x\nfunc @f version=v plan=#0\nblock b0(ptr %st, ptr %m):\n  ret %nope\n")
        .unwrap_err();
    assert!(e.message.contains("never defined"), "{e}");
}
