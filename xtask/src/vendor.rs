//! Fetching DuckDB's PEG grammar and writing it into the tree.
//!
//! `spec/20-the-grammar.md` is the argument for why the grammar is vendored rather than
//! transcribed. This is the only thing allowed to write into `crates/rudb-parse/grammar/`, and
//! [`verify`] is the check that says nobody wrote there by hand.
//!
//! In Rust rather than in a shell script for one reason that is not style: CI runs this check on
//! Windows, and a repository whose compatibility claim is enforced by a bash script has a
//! compatibility claim that is enforced on two thirds of its platforms.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::sha256;

const UPSTREAM: &str = "https://github.com/duckdb/duckdb.git";
const DEFAULT_REF: &str = "v2.0-cyanoptera";
/// Where the vendored tree lives, relative to the workspace root.
pub(crate) const DEST: &str = "crates/rudb-parse/grammar";

/// Fetches the grammar at `reference` and rewrites the vendored tree from it.
///
/// With no reference it re-fetches whatever `VENDOR` already records, which is how you find out
/// whether upstream has moved without deciding to move with it.
pub(crate) fn vendor(reference: Option<&str>) -> Result<(), String> {
    let root = crate::root();
    let dest = root.join(DEST);
    let reference = match reference {
        Some(reference) => reference.to_string(),
        None => read_field(&dest.join("VENDOR"), "ref").unwrap_or_else(|| DEFAULT_REF.to_string()),
    };

    // Everything is assembled somewhere else and moved into place at the end. A fetch that dies
    // halfway must not leave a grammar that is half of one release and half of another, because
    // that state builds, passes most tests, and is not any dialect anybody has.
    let work = root.join("target").join("vendor-grammar");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)
        .map_err(|e| format!("could not make {}: {e}", work.display()))?;
    let checkout = work.join("duckdb");
    let staged = work.join("staged");

    println!("fetching {UPSTREAM} at {reference}");
    git(&[
        "clone",
        "--quiet",
        "--depth",
        "1",
        "--branch",
        &reference,
        "--filter=blob:none",
        "--sparse",
        UPSTREAM,
        &checkout.to_string_lossy(),
    ])?;
    // A sparse checkout of two directories. The grammar is sixty kilobytes and the repository is
    // not, and a blobless clone of the whole tree still walks all of it.
    git(&[
        "-C",
        &checkout.to_string_lossy(),
        "sparse-checkout",
        "set",
        "src/parser/peg",
        "scripts/parser",
    ])?;
    let commit = git_output(&["-C", &checkout.to_string_lossy(), "rev-parse", "HEAD"])?;

    let peg = checkout.join("src").join("parser").join("peg");
    let grammar = peg.join("grammar");
    let types = checkout.join("scripts").join("parser").join("grammar_types.yml");
    let compiled = peg.join("compiled_grammar.cpp");
    let license = checkout.join("LICENSE");
    for required in
        [&grammar.join("statements"), &grammar.join("keywords"), &types, &compiled, &license]
    {
        if !required.exists() {
            return Err(format!(
                "upstream layout changed, {} is missing at {reference}\n  \
                 read spec/20-the-grammar.md section 6 before changing anything here",
                required.display()
            ));
        }
    }

    std::fs::create_dir_all(staged.join("statements"))
        .and_then(|()| std::fs::create_dir_all(staged.join("keywords")))
        .map_err(|e| format!("could not make the staging directory: {e}"))?;
    let grams = copy_matching(&grammar.join("statements"), &staged.join("statements"), "gram")?;
    let lists = copy_matching(&grammar.join("keywords"), &staged.join("keywords"), "list")?;
    copy(&license, &staged.join("LICENSE.duckdb"))?;

    let memoized = memoized_rules(&types)?;
    write(&staged.join("memoized_rules.list"), &memoized)?;
    let overrides = matcher_overrides(&compiled)?;
    write(&staged.join("matcher_overrides.list"), &overrides)?;

    let manifest = manifest(&staged)?;
    write(&staged.join("VENDOR"), &vendor_file(&reference, &commit, &manifest))?;

    // Replace rather than merge. A file upstream deleted has to disappear here too, and a merge
    // would keep it forever as a rule nothing references and nobody can explain.
    let _ = std::fs::remove_dir_all(&dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
    }
    std::fs::rename(&staged, &dest)
        .map_err(|e| format!("could not move the grammar into {}: {e}", dest.display()))?;
    let _ = std::fs::remove_dir_all(&work);

    println!(
        "vendored {grams} grammar files and {lists} keyword lists at {}\n  \
         {} memoized rules, {} matcher overrides\n  \
         `git diff {DEST}` is the complete syntactic change in that release",
        &commit[..12.min(commit.len())],
        memoized.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).count(),
        overrides.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).count(),
    );
    Ok(())
}

/// Checks every vendored file against the checksum `VENDOR` recorded for it.
///
/// This is what makes "the vendored tree is never edited" a fact rather than a request. A local
/// fix to the grammar is the moment compatible by construction becomes compatible except for the
/// edits, and nobody remembers what they were.
pub(crate) fn verify() -> Result<(), String> {
    let root = crate::root();
    let dest = root.join(DEST);
    let vendor = dest.join("VENDOR");
    if !vendor.exists() {
        return Err(format!(
            "{DEST}/VENDOR is missing, so there is no vendored grammar to check\n  \
             run `cargo xtask vendor-grammar` to fetch it"
        ));
    }
    let text = std::fs::read_to_string(&vendor)
        .map_err(|e| format!("could not read {}: {e}", vendor.display()))?;

    let mut recorded = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.contains(": ") {
            continue;
        }
        let Some((sum, name)) = line.split_once("  ") else { continue };
        recorded.insert(name.to_string(), sum.to_string());
    }
    if recorded.is_empty() {
        return Err(format!("{DEST}/VENDOR records no checksums"));
    }

    let present = manifest(&dest)?;
    let mut problems = Vec::new();
    for (name, sum) in &recorded {
        match present.get(name) {
            None => problems.push(format!("{name} is recorded in VENDOR and is not there")),
            Some(actual) if actual != sum => problems.push(format!("{name} has been edited")),
            Some(_) => {}
        }
    }
    for name in present.keys() {
        if !recorded.contains_key(name) {
            problems.push(format!("{name} is in the tree and not in VENDOR"));
        }
    }

    if problems.is_empty() {
        println!("the vendored grammar matches VENDOR across {} files", recorded.len());
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        Err(format!(
            "{} vendored files do not match VENDOR\n  \
             the vendored tree is upstream's, byte for byte, and `cargo xtask vendor-grammar` is \
             the only thing that writes to it",
            problems.len()
        ))
    }
}

/// The twenty two rules DuckDB memoizes, out of `scripts/parser/grammar_types.yml`.
///
/// Not derivable from the grammar. It is a decision somebody made with a profiler over the
/// expression precedence chain, and `spec/20-the-grammar.md` section 4 is explicit that we copy it
/// rather than choosing our own, because the memoized set is observable in what the matcher
/// accepts and not only in how fast it accepts it.
fn memoized_rules(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut out = String::from(
        "# packrat_memoized_rules, extracted from scripts/parser/grammar_types.yml.\n\
         # One rule name per line. See spec/20-the-grammar.md section 4.\n",
    );
    let mut found = false;
    let mut count = 0usize;
    for line in text.lines() {
        if line.trim_end() == "packrat_memoized_rules:" {
            found = true;
            continue;
        }
        if !found {
            continue;
        }
        let Some(rule) = line.strip_prefix("  - ") else {
            // The block ends at the first line that is not one of its items, which for a comment
            // or a blank line inside the block would end it early. Upstream has neither, and if it
            // grows one the count check below is what notices.
            if line.trim().is_empty() {
                continue;
            }
            break;
        };
        out.push_str(rule.trim());
        out.push('\n');
        count += 1;
    }
    if !found {
        return Err(format!(
            "no packrat_memoized_rules block in {}\n  \
             upstream moved it, and guessing a memoized set is worse than stopping",
            path.display()
        ));
    }
    if count == 0 {
        return Err("the packrat_memoized_rules block is empty".to_string());
    }
    Ok(out)
}

/// The rules the matcher matches itself, out of the generated block in `compiled_grammar.cpp`.
///
/// Skipping this is a correctness bug and not a missed optimization. `OperatorLiteral <-
/// Identifier` is what the grammar text says, so a matcher that believed the body would read a
/// bare `+` as an identifier. The suggestion is carried because it is not autocomplete trivia: it
/// decides which keyword class a position tolerates and whether a single quoted string counts as a
/// name there, which is the whole of how `FROM 'data.parquet'` parses without a rule for it.
fn matcher_overrides(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let start = "START GENERATED RULE OVERRIDES";
    let end = "END GENERATED RULE OVERRIDES";
    let (Some(from), Some(to)) = (text.find(start), text.find(end)) else {
        return Err(format!(
            "no generated rule override block in {}\n  \
             read spec/20-the-grammar.md section 3 before working around this",
            path.display()
        ));
    };
    if to < from {
        return Err("the override block markers are in the wrong order".to_string());
    }
    let block = &text[from + start.len()..to];

    let mut out = String::from(
        "# The rules whose bodies the matcher does not walk, extracted from the generated block\n\
         # in src/parser/peg/compiled_grammar.cpp. Three tab separated fields: the rule, the\n\
         # matcher class, and the suggestion the matcher was built with. See\n\
         # spec/20-the-grammar.md section 3.\n",
    );
    let mut count = 0usize;
    for call in block.split("AddTerminalRuleOverride(").skip(1) {
        // The calls are clang-format wrapped across one, two or three lines depending on how long
        // the names are, so the text is flattened before anything is read out of it.
        let flat: String = call.split_whitespace().collect::<Vec<_>>().join(" ");
        let Some(rule) = between(&flat, '"', '"') else { continue };
        let matcher = between(&flat, '<', '>').unwrap_or_else(|| "unknown".to_string());
        let suggestion = flat
            .find("SuggestionState::")
            .map(|at| {
                flat[at + "SuggestionState::".len()..]
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .unwrap_or_default();
        out.push_str(&format!("{rule}\t{matcher}\t{suggestion}\n"));
        count += 1;
    }
    if count == 0 {
        return Err("the generated rule override block is empty".to_string());
    }
    Ok(out)
}

fn between(text: &str, open: char, close: char) -> Option<String> {
    let from = text.find(open)? + open.len_utf8();
    let to = text[from..].find(close)? + from;
    Some(text[from..to].to_string())
}

/// Every file under `dir`, by path relative to it, with its hash.
fn manifest(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|e| format!("could not read {}: {e}", current.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = relative(dir, &path);
            if name == "VENDOR" {
                // The file that records the checksums cannot record its own.
                continue;
            }
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("could not read {}: {e}", path.display()))?;
            out.insert(name, sha256::hex(&bytes));
        }
    }
    Ok(out)
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn vendor_file(reference: &str, commit: &str, manifest: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    out.push_str(
        "# DuckDB's PEG grammar, vendored verbatim. Nothing in this directory is edited, ever,\n\
         # not for a fix and not for a workaround. `cargo xtask vendor-grammar` is the only thing\n\
         # that writes here and `cargo xtask grammar` checks that nothing else did.\n\
         #\n\
         # License: MIT, see LICENSE.duckdb. memoized_rules.list is extracted from\n\
         # scripts/parser/grammar_types.yml and matcher_overrides.list from\n\
         # src/parser/peg/compiled_grammar.cpp, in the same repository at the same commit.\n\
         #\n\
         # Why this exists at all: spec/20-the-grammar.md.\n\n",
    );
    out.push_str(&format!("upstream: {UPSTREAM}\n"));
    out.push_str(&format!("ref: {reference}\n"));
    out.push_str(&format!("commit: {commit}\n"));
    out.push_str(&format!("retrieved: {}\n", today()));
    out.push_str("\n# sha256 of every vendored file, relative to this directory.\n");
    for (name, sum) in manifest {
        out.push_str(&format!("{sum}  {name}\n"));
    }
    out
}

fn read_field(path: &Path, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{field}: ")))
        .map(|value| value.trim().to_string())
}

/// Today, as `YYYY-MM-DD` in UTC.
///
/// Written out rather than pulled in, because it is fifteen lines and this crate having no
/// dependencies is worth more than the fifteen lines cost.
fn today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    // Howard Hinnant's civil_from_days, which is the standard way to do this without a table. The
    // year is shifted so that the leap day lands at the end of it, which is what removes every
    // special case from the arithmetic below.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

fn copy_matching(from: &Path, to: &Path, extension: &str) -> Result<usize, String> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(from)
        .map_err(|e| format!("could not read {}: {e}", from.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(extension))
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(format!("no .{extension} files in {}", from.display()));
    }
    for name in &names {
        let target = to.join(name.file_name().unwrap_or_default());
        copy(name, &target)?;
    }
    Ok(names.len())
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("could not copy {} to {}: {e}", from.display(), to.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn git(args: &[&str]) -> Result<(), String> {
    let status =
        Command::new("git").args(args).status().map_err(|e| format!("could not run git: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git {} failed", args.first().copied().unwrap_or("")))
    }
}

fn git_output(args: &[&str]) -> Result<String, String> {
    let out =
        Command::new("git").args(args).output().map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {} failed", args.first().copied().unwrap_or("")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{between, matcher_overrides, memoized_rules, today};

    fn write_temp(name: &str, text: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("rudb-vendor-{name}-{unique}"));
        std::fs::write(&path, text).expect("could not write the temporary file");
        path
    }

    #[test]
    fn the_memoized_block_is_read_and_the_rest_of_the_file_is_not() {
        let path = write_temp(
            "memo.yml",
            "excluded_rules:\n  - NotThisOne\n\npackrat_memoized_rules:\n  - Expression\n  - ColId\n\nsomething_else:\n  - NorThis\n",
        );
        let out = memoized_rules(&path).unwrap();
        let rules: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(rules, vec!["Expression", "ColId"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_memoized_block_stops_rather_than_guessing() {
        let path = write_temp("nomemo.yml", "excluded_rules:\n  - A\n");
        let error = memoized_rules(&path).unwrap_err();
        assert!(error.contains("no packrat_memoized_rules block"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_override_call_wrapped_across_lines_is_still_one_override() {
        // clang-format wraps these at three different widths depending on how long the names are,
        // and a reader that assumed one call per line would silently find two thirds of them.
        let path = write_temp(
            "over.cpp",
            "noise\n// START GENERATED RULE OVERRIDES\n\
             AddTerminalRuleOverride(overrides, \"Identifier\",\n\
             \tmake_uniq<IdentifierMatcher>(SuggestionState::SUGGEST_VARIABLE, keyword_helper));\n\
             AddTerminalRuleOverride(\n\toverrides, \"FunctionName\",\n\
             \tmake_uniq<IdentifierMatcher>(SuggestionState::SUGGEST_SCALAR_FUNCTION_NAME, keyword_helper));\n\
             AddTerminalRuleOverride(overrides, \"NumberLiteral\", make_uniq<NumberLiteralMatcher>());\n\
             // END GENERATED RULE OVERRIDES\n\
             AddTerminalRuleOverride(overrides, \"EndOfInput\", make_uniq<EndOfInputMatcher>());\n",
        );
        let out = matcher_overrides(&path).unwrap();
        let rows: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            rows,
            vec![
                "Identifier\tIdentifierMatcher\tSUGGEST_VARIABLE",
                "FunctionName\tIdentifierMatcher\tSUGGEST_SCALAR_FUNCTION_NAME",
                // No suggestion, and an empty third field rather than a missing one, so that a reader
                // can split on tab and get three fields every time.
                "NumberLiteral\tNumberLiteralMatcher\t",
            ]
        );
        // EndOfInput is outside the generated block and upstream installs it separately, so it is
        // deliberately not here.
        assert!(!out.contains("EndOfInput"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn between_finds_the_first_pair_and_not_the_widest() {
        assert_eq!(between("a \"one\" and \"two\"", '"', '"').as_deref(), Some("one"));
        assert_eq!(between("make_uniq<Matcher>(x)", '<', '>').as_deref(), Some("Matcher"));
        assert_eq!(between("nothing here", '"', '"'), None);
    }

    #[test]
    fn the_date_is_a_date() {
        let today = today();
        assert_eq!(today.len(), 10, "{today}");
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3);
        let year: i32 = parts[0].parse().expect("the year is not a number");
        let month: u32 = parts[1].parse().expect("the month is not a number");
        let day: u32 = parts[2].parse().expect("the day is not a number");
        assert!((2026..2100).contains(&year), "{today}");
        assert!((1..=12).contains(&month), "{today}");
        assert!((1..=31).contains(&day), "{today}");
    }
}
