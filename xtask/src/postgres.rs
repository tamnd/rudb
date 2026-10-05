//! The PostgreSQL files that rudb uses as data, and the Rust that is generated from them.
//!
//! The files are copied from a PostgreSQL checkout at the pin, without changes. Each vendor
//! directory has a `VENDOR` file with the commit and the SHA-256 of each file. `cargo xtask
//! pg-vendor <checkout>` is the only thing that writes there, and it runs the generators in the
//! same step. `cargo xtask pg-check` checks that every vendored file matches its hash and that
//! every generated file matches what its generator writes today. The gate runs the check.
//!
//! The plan is `16-crate-layout.md` section 16.5 of the PostgreSQL compatibility notes. Today
//! there are two vendored files: `errcodes.txt` gives the SQLSTATE list of `rudb-common`, and
//! `cmdtaglist.h` gives the command tags of `rudb-pgwire`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use crate::sha256;

/// One vendor directory, relative to the workspace root, and the files copied into it as the path
/// in the checkout and the name in the tree.
struct Vendor {
    dir: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const VENDORS: [Vendor; 2] = [
    Vendor {
        dir: "crates/rudb-common/vendor",
        files: &[
            ("src/backend/utils/errcodes.txt", "errcodes.txt"),
            ("COPYRIGHT", "LICENSE.postgres"),
        ],
    },
    Vendor {
        dir: "crates/rudb-pgwire/vendor",
        files: &[
            ("src/include/tcop/cmdtaglist.h", "cmdtaglist.h"),
            ("COPYRIGHT", "LICENSE.postgres"),
        ],
    },
];

/// One generated file: where it goes, the vendored file it comes from, both relative to the
/// workspace root, and the generator.
struct Generated {
    output: &'static str,
    input: &'static str,
    generate: fn(&str) -> Result<String, String>,
}

const GENERATED: [Generated; 2] = [
    Generated {
        output: "crates/rudb-common/src/generated/sqlstate.rs",
        input: "crates/rudb-common/vendor/errcodes.txt",
        generate: sqlstate,
    },
    Generated {
        output: "crates/rudb-pgwire/src/generated/cmdtag.rs",
        input: "crates/rudb-pgwire/vendor/cmdtaglist.h",
        generate: cmdtag,
    },
];

/// Copies the files from a PostgreSQL checkout and regenerates the Rust made from them.
pub(crate) fn vendor(checkout: Option<&str>) -> Result<(), String> {
    let Some(checkout) = checkout else {
        return Err("usage: cargo xtask pg-vendor <postgres checkout at the pin>".to_string());
    };
    let checkout = Path::new(checkout);
    let commit = git(checkout, &["rev-parse", "HEAD"])?;
    let version = version(&read(&checkout.join("meson.build"))?)?;
    let root = crate::root();

    let mut copied = 0;
    for vendor in &VENDORS {
        let dir = vendor.dir;
        let dest = root.join(dir);
        std::fs::create_dir_all(&dest).map_err(|e| format!("could not make {dir}: {e}"))?;
        let mut sums = String::new();
        for (from, name) in vendor.files {
            let data = std::fs::read(checkout.join(from))
                .map_err(|e| format!("could not read {from} in {}: {e}", checkout.display()))?;
            std::fs::write(dest.join(name), &data)
                .map_err(|e| format!("could not write {dir}/{name}: {e}"))?;
            let _ = writeln!(sums, "{}  {name}", sha256::hex(&data));
            copied += 1;
        }
        let manifest = format!(
            "# PostgreSQL files, copied from the pin without changes. `cargo xtask pg-vendor` is the\n\
             # only thing that writes here and `cargo xtask pg-check` checks that nothing else did.\n\
             #\n\
             # License: the PostgreSQL License, see LICENSE.postgres.\n\
             \n\
             upstream: https://git.postgresql.org/git/postgresql.git\n\
             commit: {commit}\n\
             version: {version}\n\
             \n\
             # sha256 of every vendored file, relative to this directory.\n\
             {sums}"
        );
        std::fs::write(dest.join("VENDOR"), manifest)
            .map_err(|e| format!("could not write {dir}/VENDOR: {e}"))?;
    }

    for generated in &GENERATED {
        let text = (generated.generate)(&read(&root.join(generated.input))?)?;
        std::fs::write(root.join(generated.output), text)
            .map_err(|e| format!("could not write {}: {e}", generated.output))?;
        println!("wrote {}", generated.output);
    }
    println!("vendored {copied} files at {version} ({commit})");
    Ok(())
}

/// Checks the vendored files against `VENDOR` and the generated files against their generators.
pub(crate) fn check() -> Result<(), String> {
    let root = crate::root();
    let mut problems = Vec::new();
    for vendor in &VENDORS {
        let dir = vendor.dir;
        let dest = root.join(dir);
        let recorded = manifest(&read(&dest.join("VENDOR"))?);
        if recorded.is_empty() {
            return Err(format!("{dir}/VENDOR records no checksums"));
        }
        for (name, sum) in &recorded {
            match std::fs::read(dest.join(name)) {
                Err(_) => problems.push(format!("{dir}/{name} is in VENDOR and is not there")),
                Ok(data) if sha256::hex(&data) != *sum => {
                    problems.push(format!("{dir}/{name} is not the file VENDOR records"));
                }
                Ok(_) => {}
            }
        }
        let present = std::fs::read_dir(&dest).map_err(|e| format!("could not list {dir}: {e}"))?;
        for entry in present.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "VENDOR" && !recorded.contains_key(&name) {
                problems.push(format!("{dir}/{name} is in the tree and not in VENDOR"));
            }
        }
    }

    for generated in &GENERATED {
        let expected = (generated.generate)(&read(&root.join(generated.input))?)?;
        if read(&root.join(generated.output))?.replace("\r\n", "\n") != expected {
            problems
                .push(format!("{} is not what {} generates", generated.output, generated.input));
        }
    }

    if problems.is_empty() {
        println!("the PostgreSQL files match VENDOR and the generated files match them");
        return Ok(());
    }
    for problem in &problems {
        eprintln!("  {problem}");
    }
    Err(format!(
        "{} problems with the vendored PostgreSQL files\n  \
         run `cargo xtask pg-vendor <checkout>` with a PostgreSQL checkout at the pin and commit \
         the result. Do not edit a vendored or a generated file by hand",
        problems.len()
    ))
}

/// One code line of `errcodes.txt`.
struct Code<'a> {
    state: &'a str,
    category: &'a str,
    name: &'a str,
    condition: &'a str,
}

/// Renders the SQLSTATE list from the text of `errcodes.txt`.
///
/// The file has a section line for each class and a code line for each code. A code line has the
/// code, the category (`S`, `W` or `E`), the C macro name and the PL/pgSQL condition name. Six
/// lines have no condition name. They give a second macro name to a code that is already in the
/// file.
fn sqlstate(text: &str) -> Result<String, String> {
    let mut classes = Vec::new();
    let mut codes = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix("Section: Class ") {
            let Some((class, title)) = section.split_once(" - ") else {
                return Err(format!("errcodes.txt line {}: a section with no title", number + 1));
            };
            classes.push((class, title));
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("errcodes.txt line {}: not a code line: {line}", number + 1);
        if !(3..=4).contains(&fields.len()) {
            return Err(bad());
        }
        let state = fields[0];
        let valid = state.len() == 5
            && state.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
            && matches!(fields[1], "S" | "W" | "E");
        let Some(name) = fields[2].strip_prefix("ERRCODE_") else { return Err(bad()) };
        if !valid {
            return Err(bad());
        }
        let condition = fields.get(3).copied().unwrap_or("");
        codes.push(Code { state, category: fields[1], name, condition });
    }

    let mut out = String::from(
        "//! The SQLSTATE codes of PostgreSQL, one constant for each code line of `errcodes.txt`.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-common/vendor/errcodes.txt`.\n\
         //! Do not edit. `cargo xtask pg-check` runs in the gate and fails if this file and the\n\
         //! vendored file disagree.\n\
         \n\
         use crate::sqlstate::{Category, SqlState};\n\
         \n\
         impl SqlState {\n",
    );
    for code in &codes {
        if code.condition.is_empty() {
            let _ = writeln!(out, "    /// `{}`, a second name for this code.", code.state);
        } else {
            let _ = writeln!(out, "    /// `{}`, condition `{}`.", code.state, code.condition);
        }
        let line =
            format!("    pub const {}: Self = Self::from_bytes(*b\"{}\");", code.name, code.state);
        if line.len() <= 100 {
            let _ = writeln!(out, "{line}");
        } else {
            let _ = writeln!(
                out,
                "    pub const {}: Self =\n        Self::from_bytes(*b\"{}\");",
                code.name, code.state
            );
        }
    }
    out.push_str("}\n\n");

    let _ = writeln!(
        out,
        "/// Every code line of the file, in file order: the code, the category, the name of the\n\
         /// constant and the condition name. The condition name is empty for a second name.\n\
         pub(crate) static CODES: [(SqlState, Category, &str, &str); {}] = [",
        codes.len()
    );
    for code in &codes {
        let category = match code.category {
            "S" => "Category::Success",
            "W" => "Category::Warning",
            _ => "Category::Error",
        };
        let line = format!(
            "    (SqlState::{}, {category}, \"{}\", \"{}\"),",
            code.name, code.name, code.condition
        );
        if line.len() <= 100 {
            let _ = writeln!(out, "{line}");
        } else {
            let _ = writeln!(
                out,
                "    (\n        SqlState::{},\n        {category},\n        \"{}\",\n        \"{}\",\n    ),",
                code.name, code.name, code.condition
            );
        }
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "/// Every class, in file order: the first two characters of its codes and its title.\n\
         pub(crate) static CLASSES: [(&str, &str); {}] = [",
        classes.len()
    );
    for (class, title) in &classes {
        let line = format!("    (\"{class}\", \"{title}\"),");
        if line.len() <= 100 {
            let _ = writeln!(out, "{line}");
        } else {
            let _ = writeln!(out, "    (\n        \"{class}\",\n        \"{title}\",\n    ),");
        }
    }
    out.push_str("];\n");
    Ok(out)
}

/// Renders the command tag list from the text of `cmdtaglist.h`.
///
/// Each line that is not a comment is `PG_CMDTAG(symbol, "name", event_trigger_ok,
/// table_rewrite_ok, rowcount)`. The file keeps the lines sorted by name, so that PostgreSQL can
/// search it, and the generator refuses a file that is not sorted. The name of a variant is the
/// symbol without `CMDTAG_`, in camel case.
fn cmdtag(text: &str) -> Result<String, String> {
    let mut tags: Vec<(String, &str, [&str; 3])> = Vec::new();
    let mut comment = false;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if comment || line.starts_with("/*") {
            comment = !line.ends_with("*/");
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let bad = || format!("cmdtaglist.h line {}: not a PG_CMDTAG line: {line}", number + 1);
        let Some(args) = line.strip_prefix("PG_CMDTAG(").and_then(|l| l.strip_suffix(')')) else {
            return Err(bad());
        };
        let Some((symbol, rest)) = args.split_once(", \"") else { return Err(bad()) };
        let Some((name, flags)) = rest.split_once("\", ") else { return Err(bad()) };
        let flags: Vec<&str> = flags.split(", ").collect();
        let Some(symbol) = symbol.strip_prefix("CMDTAG_") else { return Err(bad()) };
        if flags.len() != 3
            || flags.iter().any(|f| !matches!(*f, "true" | "false"))
            || !name.bytes().all(|b| b.is_ascii_uppercase() || b == b' ' || b == b'?')
        {
            return Err(bad());
        }
        if let Some(last) = tags.last()
            && last.1 >= name
        {
            return Err(format!("cmdtaglist.h line {}: {name} is out of order", number + 1));
        }
        let variant: String = symbol
            .split('_')
            .map(|word| word[..1].to_string() + &word[1..].to_ascii_lowercase())
            .collect();
        tags.push((variant, name, [flags[0], flags[1], flags[2]]));
    }

    let mut out = String::from(
        "//! The command tags of PostgreSQL, one for each line of `cmdtaglist.h`.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-pgwire/vendor/cmdtaglist.h`.\n\
         //! Do not edit. `cargo xtask pg-check` runs in the gate and fails if this file and the\n\
         //! vendored file disagree.\n\
         \n\
         /// The tag of a statement, which `CommandComplete` sends. The variants are in the order of\n\
         /// `cmdtaglist.h`, which is the order of the names.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]\n\
         pub enum CommandTag {\n",
    );
    for (variant, name, _) in &tags {
        let _ = writeln!(out, "    /// `{name}`.\n    {variant},");
    }
    out.push_str("}\n\n");
    let _ = writeln!(
        out,
        "/// Every tag in the order of the enum: the tag, the name, and the flags `event_trigger_ok`,\n\
         /// `table_rewrite_ok` and `rowcount`.\n\
         pub(crate) static TAGS: [(CommandTag, &str, bool, bool, bool); {}] = [",
        tags.len()
    );
    for (variant, name, [event, rewrite, rows]) in &tags {
        let line = format!("    (CommandTag::{variant}, \"{name}\", {event}, {rewrite}, {rows}),");
        if line.len() <= 100 {
            let _ = writeln!(out, "{line}");
        } else {
            let _ = writeln!(
                out,
                "    (\n        CommandTag::{variant},\n        \"{name}\",\n        {event},\n        {rewrite},\n        {rows},\n    ),"
            );
        }
    }
    out.push_str("];\n");
    Ok(out)
}

/// The version in the `project()` call of the top `meson.build`, for example `19beta4`.
fn version(meson: &str) -> Result<String, String> {
    meson
        .lines()
        .find_map(|line| line.trim().strip_prefix("version: '"))
        .and_then(|rest| rest.split_once('\''))
        .map(|(version, _)| version.to_string())
        .ok_or_else(|| "meson.build has no version line".to_string())
}

fn manifest(text: &str) -> BTreeMap<String, String> {
    let mut recorded = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.contains(": ") {
            continue;
        }
        if let Some((sum, name)) = line.split_once("  ") {
            recorded.insert(name.to_string(), sum.to_string());
        }
    }
    recorded
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))
}

fn git(checkout: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            checkout.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{cmdtag, manifest, sqlstate, version};

    const SAMPLE: &str = "\
# A comment.

Section: Class 00 - Successful Completion

00000    S    ERRCODE_SUCCESSFUL_COMPLETION                                  successful_completion
Section: Class 3D - Invalid Catalog Name

3D000    E    ERRCODE_INVALID_CATALOG_NAME                                   invalid_catalog_name
3D000    E    ERRCODE_UNDEFINED_DATABASE
";

    #[test]
    fn the_sample_generates_constants_codes_and_classes() {
        let out = sqlstate(SAMPLE).expect("the sample parses");
        assert!(out.contains(
            "    pub const SUCCESSFUL_COMPLETION: Self = Self::from_bytes(*b\"00000\");"
        ));
        assert!(out.contains("    /// `3D000`, a second name for this code.\n"));
        assert!(out.contains("CODES: [(SqlState, Category, &str, &str); 3]"));
        assert!(out.contains(
            "    (SqlState::UNDEFINED_DATABASE, Category::Error, \"UNDEFINED_DATABASE\", \"\"),"
        ));
        assert!(out.contains("    (\"3D\", \"Invalid Catalog Name\"),"));
    }

    #[test]
    fn a_line_that_is_not_a_code_is_refused() {
        assert!(sqlstate("4200 E ERRCODE_X x\n").is_err());
        assert!(sqlstate("42000 X ERRCODE_X x\n").is_err());
        assert!(sqlstate("42000 E X x\n").is_err());
    }

    #[test]
    fn the_version_comes_from_the_project_call() {
        let meson = "project('postgresql',\n  ['c'],\n  version: '19beta4',\n)\n";
        assert_eq!(version(meson).as_deref(), Ok("19beta4"));
        assert!(version("project('x')\n").is_err());
    }

    #[test]
    fn the_manifest_skips_comments_and_header_fields() {
        let found = manifest("# c\ncommit: abc\n\nffff  errcodes.txt\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found.get("errcodes.txt").map(String::as_str), Some("ffff"));
    }

    #[test]
    fn the_command_tags_become_variants_and_rows() {
        let text = "/*\n * a comment\n */\n\n\
                    PG_CMDTAG(CMDTAG_UNKNOWN, \"???\", false, false, false)\n\
                    PG_CMDTAG(CMDTAG_ALTER_ACCESS_METHOD, \"ALTER ACCESS METHOD\", true, false, false)\n\
                    PG_CMDTAG(CMDTAG_INSERT, \"INSERT\", false, false, true)\n";
        let out = cmdtag(text).expect("the sample parses");
        assert!(out.contains("    /// `???`.\n    Unknown,\n"));
        assert!(out.contains("    AlterAccessMethod,\n"));
        assert!(out.contains("TAGS: [(CommandTag, &str, bool, bool, bool); 3]"));
        assert!(out.contains("    (CommandTag::Insert, \"INSERT\", false, false, true),\n"));
    }

    #[test]
    fn a_command_tag_out_of_order_is_refused() {
        let text = "PG_CMDTAG(CMDTAG_INSERT, \"INSERT\", false, false, true)\n\
                    PG_CMDTAG(CMDTAG_DELETE, \"DELETE\", false, false, true)\n";
        assert!(cmdtag(text).is_err());
        assert!(cmdtag("PG_CMDTAG(CMDTAG_X, \"X\", maybe, false, true)\n").is_err());
    }
}
