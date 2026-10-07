//! The PostgreSQL files that rudb uses as data, and the Rust that is generated from them.
//!
//! The files are copied from a PostgreSQL checkout at the pin, without changes. Each vendor
//! directory has a `VENDOR` file with the commit and the SHA-256 of each file. `cargo xtask
//! pg-vendor <checkout>` is the only thing that writes there, and it runs the generators in the
//! same step. `cargo xtask pg-check` checks that every vendored file matches its hash and that
//! every generated file matches what its generator writes today. The gate runs the check.
//!
//! The plan is `16-crate-layout.md` section 16.5 of the PostgreSQL compatibility notes. Today
//! `errcodes.txt` gives the SQLSTATE list of `rudb-common`, `guc_parameters.dat` and
//! `guc_tables.c` give the configuration parameters of `rudb-common`, `cmdtaglist.h` gives the
//! command tags of `rudb-pgwire`, `pg_type.dat` gives the type OIDs of `rudb-pgtypes`, and the
//! samples of `pg_hba.conf`, `pg_ident.conf` and `postgresql.conf` are the files that
//! `rudb-server init` writes.
//!
//! `gram.y` gives the parse tables of `rudb-pgparse` through `postgres/gram.rs`, which removes the
//! C, and `postgres/lalr.rs`, which makes the LALR(1) tables as bison 2.3 makes them. `kwlist.h`
//! gives its keywords. `nodes.h`, `lockoptions.h`, `primnodes.h`, `parsenodes.h` and `value.h`
//! give the node types of its raw parse tree through `postgres/nodes.rs`. `scan.l` and `parser.c`
//! are the reference for its lexer. `cargo xtask pg-grammar` runs bison on the same grammar and
//! compares the two sets of tables entry by entry.

mod glue;
mod gram;
mod guc;
mod lalr;
mod nodes;
mod pgparse;
mod translate;

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

const VENDORS: [Vendor; 5] = [
    Vendor {
        dir: "crates/rudb-common/vendor",
        files: &[
            ("src/backend/utils/errcodes.txt", "errcodes.txt"),
            ("src/backend/utils/misc/guc_parameters.dat", "guc_parameters.dat"),
            ("src/backend/utils/misc/guc_tables.c", "guc_tables.c"),
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
    Vendor {
        dir: "crates/rudb-pgtypes/vendor",
        files: &[
            ("src/include/catalog/pg_type.dat", "pg_type.dat"),
            ("src/include/catalog/pg_cast.dat", "pg_cast.dat"),
            ("src/timezone/tznames/Default", "tznames-Default"),
            ("COPYRIGHT", "LICENSE.postgres"),
        ],
    },
    Vendor {
        dir: "crates/rudb-pgparse/vendor",
        files: &[
            ("src/backend/parser/gram.y", "gram.y"),
            ("src/backend/parser/scan.l", "scan.l"),
            ("src/backend/parser/parser.c", "parser.c"),
            ("src/include/parser/kwlist.h", "kwlist.h"),
            ("src/include/nodes/nodes.h", "nodes.h"),
            ("src/include/nodes/lockoptions.h", "lockoptions.h"),
            ("src/include/nodes/primnodes.h", "primnodes.h"),
            ("src/include/nodes/parsenodes.h", "parsenodes.h"),
            ("src/include/nodes/value.h", "value.h"),
            ("src/include/catalog/pg_class.h", "pg_class.h"),
            ("src/include/catalog/pg_am.h", "pg_am.h"),
            ("src/include/catalog/pg_attribute.h", "pg_attribute.h"),
            ("src/include/catalog/pg_trigger.h", "pg_trigger.h"),
            ("src/include/catalog/index.h", "index.h"),
            ("src/include/commands/trigger.h", "trigger.h"),
            ("src/include/utils/datetime.h", "datetime.h"),
            ("src/include/utils/timestamp.h", "timestamp.h"),
            ("src/include/utils/xml.h", "xml.h"),
            ("src/include/storage/lockdefs.h", "lockdefs.h"),
            ("src/include/common/relpath.h", "relpath.h"),
            ("COPYRIGHT", "LICENSE.postgres"),
        ],
    },
    Vendor {
        dir: "crates/rudb-server/vendor",
        files: &[
            ("src/backend/libpq/pg_hba.conf.sample", "pg_hba.conf.sample"),
            ("src/backend/libpq/pg_ident.conf.sample", "pg_ident.conf.sample"),
            ("src/backend/utils/misc/postgresql.conf.sample", "postgresql.conf.sample"),
            ("COPYRIGHT", "LICENSE.postgres"),
        ],
    },
];

/// One generated file: where it goes, the vendored files it comes from, all relative to the
/// workspace root, and the generator, which gets the text of the files in the same order.
struct Generated {
    output: &'static str,
    inputs: &'static [&'static str],
    generate: fn(&[String]) -> Result<String, String>,
}

const GENERATED: [Generated; 11] = [
    Generated {
        output: "crates/rudb-common/src/generated/sqlstate.rs",
        inputs: &["crates/rudb-common/vendor/errcodes.txt"],
        generate: |texts| sqlstate(&texts[0]),
    },
    Generated {
        output: "crates/rudb-common/src/generated/guc.rs",
        inputs: &[
            "crates/rudb-common/vendor/guc_parameters.dat",
            "crates/rudb-common/vendor/guc_tables.c",
        ],
        generate: guc::guc,
    },
    Generated {
        output: "crates/rudb-pgwire/src/generated/cmdtag.rs",
        inputs: &["crates/rudb-pgwire/vendor/cmdtaglist.h"],
        generate: |texts| cmdtag(&texts[0]),
    },
    Generated {
        output: "crates/rudb-pgtypes/src/generated/oids.rs",
        inputs: &["crates/rudb-pgtypes/vendor/pg_type.dat"],
        generate: |texts| pgtype(&texts[0]),
    },
    Generated {
        output: "crates/rudb-pgtypes/src/generated/casts.rs",
        inputs: &[
            "crates/rudb-pgtypes/vendor/pg_type.dat",
            "crates/rudb-pgtypes/vendor/pg_cast.dat",
        ],
        generate: |texts| pgcast(&texts[0], &texts[1]),
    },
    Generated {
        output: "crates/rudb-pgparse/src/generated/gram.rules",
        inputs: &["crates/rudb-pgparse/vendor/gram.y"],
        generate: |texts| gram::rules(&texts[0]),
    },
    Generated {
        output: "crates/rudb-pgparse/src/generated/productions.txt",
        inputs: &["crates/rudb-pgparse/vendor/gram.y"],
        generate: |texts| gram::productions(&texts[0]),
    },
    Generated {
        output: "crates/rudb-pgparse/src/generated/tables.rs",
        inputs: &["crates/rudb-pgparse/vendor/gram.y"],
        generate: pgparse::tables,
    },
    Generated {
        output: "crates/rudb-pgparse/src/generated/keywords.rs",
        inputs: &["crates/rudb-pgparse/vendor/kwlist.h", "crates/rudb-pgparse/vendor/gram.y"],
        generate: pgparse::keywords,
    },
    Generated {
        output: "crates/rudb-pgparse/src/generated/nodes.rs",
        inputs: &[
            "crates/rudb-pgparse/vendor/nodes.h",
            "crates/rudb-pgparse/vendor/lockoptions.h",
            "crates/rudb-pgparse/vendor/primnodes.h",
            "crates/rudb-pgparse/vendor/parsenodes.h",
            "crates/rudb-pgparse/vendor/value.h",
            "crates/rudb-pgparse/vendor/pg_class.h",
            "crates/rudb-pgparse/vendor/pg_am.h",
            "crates/rudb-pgparse/vendor/pg_attribute.h",
            "crates/rudb-pgparse/vendor/pg_trigger.h",
            "crates/rudb-pgparse/vendor/index.h",
            "crates/rudb-pgparse/vendor/trigger.h",
            "crates/rudb-pgparse/vendor/datetime.h",
            "crates/rudb-pgparse/vendor/timestamp.h",
            "crates/rudb-pgparse/vendor/xml.h",
            "crates/rudb-pgparse/vendor/lockdefs.h",
            "crates/rudb-pgparse/vendor/relpath.h",
            "crates/rudb-common/vendor/errcodes.txt",
            "crates/rudb-pgtypes/vendor/pg_type.dat",
            "crates/rudb-pgparse/vendor/gram.y",
        ],
        generate: nodes::nodes,
    },
    Generated {
        output: "crates/rudb-pgparse/src/generated/glue.rs",
        inputs: &[
            "crates/rudb-pgparse/vendor/nodes.h",
            "crates/rudb-pgparse/vendor/lockoptions.h",
            "crates/rudb-pgparse/vendor/primnodes.h",
            "crates/rudb-pgparse/vendor/parsenodes.h",
            "crates/rudb-pgparse/vendor/value.h",
            "crates/rudb-pgparse/vendor/pg_class.h",
            "crates/rudb-pgparse/vendor/pg_am.h",
            "crates/rudb-pgparse/vendor/pg_attribute.h",
            "crates/rudb-pgparse/vendor/pg_trigger.h",
            "crates/rudb-pgparse/vendor/index.h",
            "crates/rudb-pgparse/vendor/trigger.h",
            "crates/rudb-pgparse/vendor/datetime.h",
            "crates/rudb-pgparse/vendor/timestamp.h",
            "crates/rudb-pgparse/vendor/xml.h",
            "crates/rudb-pgparse/vendor/lockdefs.h",
            "crates/rudb-pgparse/vendor/relpath.h",
            "crates/rudb-common/vendor/errcodes.txt",
            "crates/rudb-pgtypes/vendor/pg_type.dat",
            "crates/rudb-pgparse/vendor/gram.y",
            "crates/rudb-pgparse/src/actions/",
        ],
        generate: glue::glue,
    },
];

/// The text of the inputs of a generated file. The text of an input that ends with `/` is the
/// text of the `.rs` files in that directory, in the order of their names.
fn inputs(root: &Path, generated: &Generated) -> Result<Vec<String>, String> {
    generated
        .inputs
        .iter()
        .map(|input| {
            if !input.ends_with('/') {
                return read(&root.join(input));
            }
            let dir = root.join(input);
            let entries =
                std::fs::read_dir(&dir).map_err(|e| format!("could not list {input}: {e}"))?;
            let mut paths: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|e| e == "rs"))
                .collect();
            paths.sort();
            let texts: Vec<String> =
                paths.iter().map(|path| read(path)).collect::<Result<_, _>>()?;
            Ok(texts.concat())
        })
        .collect()
}

/// Regenerates the Rust made from the vendored files, without a PostgreSQL checkout. A port of an
/// action of `gram.y` in `src/actions` needs this, because the glue lists the ported rules.
pub(crate) fn generate() -> Result<(), String> {
    let root = crate::root();
    for generated in &GENERATED {
        let text = (generated.generate)(&inputs(&root, generated)?)?;
        let path = root.join(generated.output);
        if read(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&path, text)
                .map_err(|e| format!("could not write {}: {e}", generated.output))?;
            println!("wrote {}", generated.output);
        }
    }
    Ok(())
}

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

    generate()?;
    println!("vendored {copied} files at {version} ({commit})");
    Ok(())
}

/// Runs bison on the PostgreSQL grammar without its C and compares its tables with the generated
/// tables of `rudb-pgparse`, entry by entry.
pub(crate) fn grammar() -> Result<(), String> {
    pgparse::compare_with_bison()
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
        for entry in present.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "VENDOR" && !recorded.contains_key(&name) {
                problems.push(format!("{dir}/{name} is in the tree and not in VENDOR"));
            }
        }
    }

    for generated in &GENERATED {
        let expected = (generated.generate)(&inputs(&root, generated)?)?;
        if read(&root.join(generated.output))?.replace("\r\n", "\n") != expected {
            let from = generated.inputs.join(" and ");
            problems.push(format!("{} is not what {from} generates", generated.output));
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
         run `cargo xtask pg-generate`, or `cargo xtask pg-vendor <checkout>` with a PostgreSQL \
         checkout at the pin, and commit the result. Do not edit a vendored or a generated file by \
         hand",
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

/// One type of `pg_type.dat` with the fields that the generated table keeps. `elem` is a type
/// name until all the entries are read, because the file refers to the element type by name.
struct TypeRow {
    oid: u32,
    name: String,
    descr: String,
    kind: char,
    category: char,
    len: i16,
    elem: String,
    array: String,
    delim: char,
}

/// Reads the entries of a catalog `.dat` file. The file is a Perl array of hashes: each entry is
/// `{ key => 'value', ... }`, a value is in single quotes with `\'` and `\\` as escapes as in Perl,
/// and a `#` outside a value starts a comment to the end of the line.
fn dat_entries(file: &str, text: &str) -> Result<Vec<BTreeMap<String, String>>, String> {
    let text: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    let mut chars = text.chars().peekable();
    let mut entries = Vec::new();
    let skip = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
        while let Some(c) = chars.next_if(|c| c.is_whitespace() || *c == '#') {
            if c == '#' {
                while chars.next_if(|c| *c != '\n').is_some() {}
            }
        }
    };
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        let mut entry = BTreeMap::new();
        loop {
            skip(&mut chars);
            if chars.next_if_eq(&'}').is_some() {
                break;
            }
            let mut key = String::new();
            while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
                key.push(c);
            }
            skip(&mut chars);
            if key.is_empty() || chars.next() != Some('=') || chars.next() != Some('>') {
                return Err(format!("{file}: a field with no `=>` after `{key}`"));
            }
            skip(&mut chars);
            if chars.next() != Some('\'') {
                return Err(format!("{file}: the value of `{key}` is not in quotes"));
            }
            let mut value = String::new();
            loop {
                match chars.next() {
                    None => return Err(format!("{file}: the value of `{key}` has no end")),
                    // Perl reads `\\` and `\'` as escapes and keeps any other backslash.
                    Some('\\') => match chars.next() {
                        Some(c @ ('\\' | '\'')) => value.push(c),
                        Some(c) => value.extend(['\\', c]),
                        None => value.push('\\'),
                    },
                    Some('\'') => break,
                    Some(c) => value.push(c),
                }
            }
            if entry.insert(key.clone(), value).is_some() {
                return Err(format!("{file}: `{key}` is two times in one entry"));
            }
            skip(&mut chars);
            let _ = chars.next_if_eq(&',');
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Renders the type OIDs from the text of `pg_type.dat`.
///
/// An entry with `array_type_oid` also gives an array type, as `genbki.pl` makes it: the name
/// with `_` in front, `typtype` `b`, `typcategory` `A`, `typlen` -1, the entry as `typelem` and
/// the `typdelim` of the entry. An entry can also name its array type with `typarray`, as
/// `record` does. The constant of a type is its name in upper case, and the constant
/// of an array is the name of its element with `_ARRAY` after it.
fn pgtype(text: &str) -> Result<String, String> {
    let mut rows = Vec::new();
    for entry in dat_entries("pg_type.dat", text)? {
        let field = |key: &str| entry.get(key).map(String::as_str);
        let name = field("typname").ok_or("pg_type.dat: an entry with no typname")?;
        let bad = |what: &str| format!("pg_type.dat: {name} has a bad {what}");
        let char_of = |key: &str, default: char| match field(key) {
            None => Ok(default),
            Some(v) if v.chars().count() == 1 => Ok(v.chars().next().unwrap_or(default)),
            Some(_) => Err(bad(key)),
        };
        let oid = field("oid").and_then(|v| v.parse().ok()).ok_or_else(|| bad("oid"))?;
        let len = match field("typlen") {
            // The server is 64-bit, and `pg_type.typlen` shows 8 there.
            Some("NAMEDATALEN") => 64,
            Some("SIZEOF_POINTER") => 8,
            Some(v) => v.parse().map_err(|_| bad("typlen"))?,
            None => return Err(bad("typlen")),
        };
        let row = TypeRow {
            oid,
            name: name.to_string(),
            descr: field("descr").unwrap_or("").to_string(),
            kind: char_of("typtype", 'b')?,
            category: char_of("typcategory", ' ')?,
            len,
            elem: field("typelem").unwrap_or("").to_string(),
            array: field("typarray").unwrap_or("").to_string(),
            delim: char_of("typdelim", ',')?,
        };
        if row.category == ' ' {
            return Err(bad("typcategory"));
        }
        let array = match field("array_type_oid") {
            Some(v) => Some(v.parse().map_err(|_| bad("array_type_oid"))?),
            None => None,
        };
        if let Some(array) = array {
            rows.push(TypeRow {
                oid: array,
                name: format!("_{name}"),
                descr: String::new(),
                kind: 'b',
                category: 'A',
                len: -1,
                elem: name.to_string(),
                array: String::new(),
                delim: row.delim,
            });
            rows.push(TypeRow { array: format!("_{name}"), ..row });
        } else {
            rows.push(row);
        }
    }

    let oids: BTreeMap<&str, u32> = rows.iter().map(|r| (r.name.as_str(), r.oid)).collect();
    if oids.len() != rows.len() {
        return Err("pg_type.dat: two types have the same name".to_string());
    }
    let resolve = |name: &str| match name {
        "" => Ok(0),
        name => oids.get(name).copied().ok_or_else(|| format!("pg_type.dat: no type {name}")),
    };
    let mut links = Vec::new();
    for row in &rows {
        links.push((resolve(&row.elem)?, resolve(&row.array)?));
    }
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rows[i].oid);
    if order.windows(2).any(|w| rows[w[0]].oid == rows[w[1]].oid) {
        return Err("pg_type.dat: two types have the same OID".to_string());
    }

    let mut out = String::from(
        "//! The built-in types of PostgreSQL: one for each entry of `pg_type.dat`, and one for each\n\
         //! array type that an entry asks for with `array_type_oid`, as `genbki.pl` makes them.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-pgtypes/vendor/pg_type.dat`.\n\
         //! Do not edit. `cargo xtask pg-check` runs in the gate and fails if this file and the\n\
         //! vendored file disagree.\n\
         \n\
         use crate::types::{Oid, TypeInfo, t};\n\
         \n",
    );
    for &i in &order {
        let row = &rows[i];
        let constant = match row.name.strip_prefix('_') {
            Some(elem) => format!("{}_ARRAY", elem.to_ascii_uppercase()),
            None => row.name.to_ascii_uppercase(),
        };
        if row.descr.is_empty() && row.category == 'A' && row.name.starts_with('_') {
            let _ = writeln!(out, "/// `{}`, the array of `{}`.", row.name, &row.name[1..]);
        } else if row.descr.is_empty() {
            let _ = writeln!(out, "/// `{}`.", row.name);
        } else {
            // A description such as `format '[point1,point2]'` must not read as a link or a tag.
            let mut descr = String::new();
            for c in row.descr.chars() {
                if "[]<>".contains(c) {
                    descr.push('\\');
                }
                descr.push(c);
            }
            let _ = writeln!(out, "/// `{}`, {descr}.", row.name);
        }
        let _ = writeln!(out, "pub const {constant}: Oid = {};", row.oid);
    }
    let _ = writeln!(
        out,
        "\n/// Every type in the order of the OIDs: the OID, `typname`, `typtype`, `typcategory`,\n\
         /// `typlen`, `typelem`, `typarray` and `typdelim`.\n\
         pub(crate) static TYPES: [TypeInfo; {}] = [",
        rows.len()
    );
    for &i in &order {
        let row = &rows[i];
        let _ = writeln!(
            out,
            "    t({}, \"{}\", b'{}', b'{}', {}, {}, {}, b'{}'),",
            row.oid, row.name, row.kind, row.category, row.len, links[i].0, links[i].1, row.delim
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// Renders the preferred types of `pg_type.dat` and the implicit casts of `pg_cast.dat`, which
/// are what `select_common_type` of PostgreSQL reads. `pg_cast.dat` names each type by its
/// `typname`.
fn pgcast(types: &str, casts: &str) -> Result<String, String> {
    let mut oids = BTreeMap::new();
    let mut preferred = Vec::new();
    for entry in dat_entries("pg_type.dat", types)? {
        let name = entry.get("typname").ok_or("pg_type.dat: an entry with no typname")?;
        let oid: u32 = entry
            .get("oid")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("pg_type.dat: {name} has a bad oid"))?;
        if entry.get("typispreferred").map(String::as_str) == Some("t") {
            preferred.push(oid);
        }
        oids.insert(name.clone(), oid);
    }
    let mut implicit = Vec::new();
    let mut assignment = Vec::new();
    for entry in dat_entries("pg_cast.dat", casts)? {
        let oid = |key: &str| {
            let name =
                entry.get(key).ok_or_else(|| format!("pg_cast.dat: a cast with no {key}"))?;
            oids.get(name).copied().ok_or_else(|| format!("pg_cast.dat: no type {name}"))
        };
        let (source, target) = (oid("castsource")?, oid("casttarget")?);
        match entry.get("castcontext").map(String::as_str) {
            Some("i") => implicit.push((source, target)),
            Some("a") => assignment.push((source, target)),
            Some("e") => {}
            _ => {
                return Err(format!(
                    "pg_cast.dat: the cast {source} to {target} has a bad castcontext"
                ));
            }
        }
    }
    preferred.sort_unstable();
    implicit.sort_unstable();
    assignment.sort_unstable();
    let mut both = [implicit.as_slice(), assignment.as_slice()].concat();
    both.sort_unstable();
    if both.windows(2).any(|w| w[0] == w[1]) {
        return Err("pg_cast.dat: two casts have the same types".to_string());
    }
    let mut out = String::from(
        "//! The preferred types of `pg_type.dat` and the implicit and assignment casts of\n\
         //! `pg_cast.dat`.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-pgtypes/vendor/pg_type.dat` and\n\
         //! `crates/rudb-pgtypes/vendor/pg_cast.dat`. Do not edit. `cargo xtask pg-check` runs in the\n\
         //! gate and fails if this file and the vendored files disagree.\n\
         \n\
         use crate::types::Oid;\n\
         \n",
    );
    let _ = writeln!(
        out,
        "/// The types with `typispreferred`, in the order of the OIDs.\n\
         pub(crate) static PREFERRED: [Oid; {}] = {preferred:?};\n\
         \n\
         /// Every cast with `castcontext` `i`, as the source and the target, in order.\n\
         pub(crate) static IMPLICIT: [(Oid, Oid); {}] = [",
        preferred.len(),
        implicit.len()
    );
    for (source, target) in implicit {
        let _ = writeln!(out, "    ({source}, {target}),");
    }
    let _ = writeln!(
        out,
        "];\n\
         \n\
         /// Every cast with `castcontext` `a`, as the source and the target, in order.\n\
         pub(crate) static ASSIGNMENT: [(Oid, Oid); {}] = [",
        assignment.len()
    );
    for (source, target) in assignment {
        let _ = writeln!(out, "    ({source}, {target}),");
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
    use super::{cmdtag, manifest, pgcast, pgtype, sqlstate, version};

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

    #[test]
    fn the_types_and_their_arrays_become_constants_and_rows() {
        let text = "# a comment\n[\n\
                    { oid => '16', array_type_oid => '1000',\n  descr => 'boolean, format \\'t\\'/\\'f\\'',\n  \
                    typname => 'bool', typlen => '1', typcategory => 'B' },\n\
                    { oid => '19', typname => 'name', typlen => 'NAMEDATALEN',\n  typcategory => 'S', typelem => 'char' },\n\
                    { oid => '18', typname => 'char', typlen => '1', typcategory => 'Z' },\n\
                    { oid => '603', array_type_oid => '1020', typname => 'box', typlen => '32',\n  \
                    typcategory => 'G', typdelim => ';' },\n\
                    ]\n";
        let out = pgtype(text).expect("the sample parses");
        assert!(out.contains("/// `bool`, boolean, format 't'/'f'.\npub const BOOL: Oid = 16;\n"));
        assert!(
            out.contains("/// `_bool`, the array of `bool`.\npub const BOOL_ARRAY: Oid = 1000;\n")
        );
        assert!(out.contains("TYPES: [TypeInfo; 6]"));
        assert!(out.contains("    t(16, \"bool\", b'b', b'B', 1, 0, 1000, b','),\n"));
        assert!(out.contains("    t(19, \"name\", b'b', b'S', 64, 18, 0, b','),\n"));
        assert!(out.contains("    t(1020, \"_box\", b'b', b'A', -1, 603, 0, b';'),\n"));
        // The rows are in the order of the OIDs.
        assert!(out.find("t(18,") < out.find("t(19,"));
    }

    #[test]
    fn the_preferred_types_and_the_implicit_casts_become_tables() {
        let types = "{ oid => '20', typname => 'int8' },\n\
                     { oid => '23', typname => 'int4' },\n\
                     { oid => '701', typname => 'float8', typispreferred => 't' },";
        let casts = "{ castsource => 'int4', casttarget => 'int8', castfunc => 'int8(int4)',\n  \
                     castcontext => 'i', castmethod => 'f' },\n\
                     { castsource => 'int8', casttarget => 'int4', castfunc => 'int4(int8)',\n  \
                     castcontext => 'a', castmethod => 'f' },\n\
                     { castsource => 'int4', casttarget => 'float8', castfunc => 'float8(int4)',\n  \
                     castcontext => 'i', castmethod => 'f' },";
        let out = pgcast(types, casts).expect("the sample parses");
        assert!(out.contains("PREFERRED: [Oid; 1] = [701];"));
        assert!(out.contains("IMPLICIT: [(Oid, Oid); 2] = [\n    (23, 20),\n    (23, 701),\n];"));
        assert!(out.contains("ASSIGNMENT: [(Oid, Oid); 1] = [\n    (20, 23),\n];"));
        assert!(
            pgcast(types, "{ castsource => 'int4', casttarget => 'x', castcontext => 'i' }")
                .is_err()
        );
    }

    #[test]
    fn a_type_with_an_unknown_element_or_a_second_oid_is_refused() {
        let elem =
            "{ oid => '1', typname => 'a', typlen => '1', typcategory => 'A', typelem => 'b' }";
        assert!(pgtype(elem).is_err());
        let twice = "{ oid => '1', typname => 'a', typlen => '1', typcategory => 'A' },\n\
                     { oid => '1', typname => 'b', typlen => '1', typcategory => 'A' }";
        assert!(pgtype(twice).is_err());
        assert!(pgtype("{ oid => '1', typname => 'a', typlen => '1' }").is_err());
    }
}
