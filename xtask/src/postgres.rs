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
//! command tags of `rudb-pgwire`, `pg_type.dat` gives the type OIDs of `rudb-pgtypes`,
//! `pg_proc.dat` and `pg_operator.dat` give its functions and its operators,
//! `unicode_norm_table.h` gives the Unicode normalization tables of `rudb-kernels`,
//! `unicode_case_table.h`, `unicode_category_table.h` and `unicode_category.h` give its Unicode
//! case and category tables, and the
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
mod norm;
mod pgparse;
mod translate;
mod unicode;

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

const VENDORS: [Vendor; 6] = [
    Vendor {
        dir: "crates/rudb-common/vendor",
        files: &[
            ("src/backend/utils/errcodes.txt", "errcodes.txt"),
            ("src/backend/utils/misc/guc_parameters.dat", "guc_parameters.dat"),
            ("src/backend/utils/misc/guc_tables.c", "guc_tables.c"),
            ("src/timezone/data/tzdata.zi", "tzdata.zi"),
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
            ("src/include/catalog/pg_proc.dat", "pg_proc.dat"),
            ("src/include/catalog/pg_operator.dat", "pg_operator.dat"),
            ("src/include/catalog/pg_collation.dat", "pg_collation.dat"),
            ("src/backend/catalog/system_functions.sql", "system_functions.sql"),
            ("src/timezone/tznames/Default", "tznames-Default"),
            ("COPYRIGHT", "LICENSE.postgres"),
        ],
    },
    Vendor {
        dir: "crates/rudb-kernels/vendor",
        files: &[
            ("src/include/common/unicode_norm_table.h", "unicode_norm_table.h"),
            ("src/include/common/unicode_case_table.h", "unicode_case_table.h"),
            ("src/include/common/unicode_category_table.h", "unicode_category_table.h"),
            ("src/include/common/unicode_category.h", "unicode_category.h"),
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

const GENERATED: [Generated; 16] = [
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
        inputs: &[
            "crates/rudb-pgtypes/vendor/pg_type.dat",
            "crates/rudb-pgtypes/vendor/pg_collation.dat",
        ],
        generate: |texts| pgtype(&texts[0], &texts[1]),
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
        output: "crates/rudb-pgtypes/src/generated/procs.rs",
        inputs: &[
            "crates/rudb-pgtypes/vendor/pg_type.dat",
            "crates/rudb-pgtypes/vendor/pg_proc.dat",
            "crates/rudb-pgtypes/vendor/system_functions.sql",
        ],
        generate: |texts| pgproc(&texts[0], &texts[1], &texts[2]),
    },
    Generated {
        output: "crates/rudb-pgtypes/src/generated/operators.rs",
        inputs: &[
            "crates/rudb-pgtypes/vendor/pg_type.dat",
            "crates/rudb-pgtypes/vendor/pg_operator.dat",
            "crates/rudb-pgtypes/vendor/pg_proc.dat",
        ],
        generate: |texts| pgoperator(&texts[0], &texts[1], &texts[2]),
    },
    Generated {
        output: "crates/rudb-pgtypes/src/generated/collations.rs",
        inputs: &["crates/rudb-pgtypes/vendor/pg_collation.dat"],
        generate: |texts| pgcollation(&texts[0]),
    },
    Generated {
        output: "crates/rudb-kernels/src/pgnormalize/table.rs",
        inputs: &["crates/rudb-kernels/vendor/unicode_norm_table.h"],
        generate: norm::tables,
    },
    Generated {
        output: "crates/rudb-kernels/src/pgunicode/table.rs",
        inputs: &[
            "crates/rudb-kernels/vendor/unicode_case_table.h",
            "crates/rudb-kernels/vendor/unicode_category_table.h",
            "crates/rudb-kernels/vendor/unicode_category.h",
        ],
        generate: unicode::tables,
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
    /// The `collname` of `typcollation`, or empty for a type that has no collation.
    collation: String,
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
/// of an array is the name of its element with `_ARRAY` after it. An array has the collation of
/// its element, and `typcollation` names an entry of `pg_collation.dat`.
fn pgtype(text: &str, collations: &str) -> Result<String, String> {
    let mut collation_oids = BTreeMap::new();
    for entry in dat_entries("pg_collation.dat", collations)? {
        let name = entry.get("collname").ok_or("pg_collation.dat: an entry with no collname")?;
        let oid: u32 = entry
            .get("oid")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("pg_collation.dat: {name} has a bad oid"))?;
        collation_oids.insert(name.clone(), oid);
    }
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
            collation: field("typcollation").unwrap_or("").to_string(),
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
                collation: row.collation.clone(),
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
        let collation = match row.collation.as_str() {
            "" => 0,
            name => *collation_oids
                .get(name)
                .ok_or_else(|| format!("pg_type.dat: no collation {name}"))?,
        };
        links.push((resolve(&row.elem)?, resolve(&row.array)?, collation));
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
         /// `typlen`, `typelem`, `typarray`, `typdelim` and `typcollation`.\n\
         pub(crate) static TYPES: [TypeInfo; {}] = [",
        rows.len()
    );
    for &i in &order {
        let row = &rows[i];
        let _ = writeln!(
            out,
            "    t({}, \"{}\", b'{}', b'{}', {}, {}, {}, b'{}', {}),",
            row.oid,
            row.name,
            row.kind,
            row.category,
            row.len,
            links[i].0,
            links[i].1,
            row.delim,
            links[i].2
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// The OID of each `typname` of `pg_type.dat`, with the array types that `array_type_oid` asks
/// for under the name of the element with `_` in front, as `genbki.pl` names them.
fn type_oids(types: &str) -> Result<BTreeMap<String, u32>, String> {
    let mut oids = BTreeMap::new();
    for entry in dat_entries("pg_type.dat", types)? {
        let name = entry.get("typname").ok_or("pg_type.dat: an entry with no typname")?;
        let oid = |key: &str| {
            entry
                .get(key)
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or_else(|| format!("pg_type.dat: {name} has a bad {key}"))
        };
        oids.insert(name.clone(), oid("oid")?);
        if entry.contains_key("array_type_oid") {
            oids.insert(format!("_{name}"), oid("array_type_oid")?);
        }
    }
    Ok(oids)
}

/// Renders the preferred types of `pg_type.dat` and every cast of `pg_cast.dat`, which are what
/// `select_common_type` and `find_coercion_pathway` of PostgreSQL read. `pg_cast.dat` names each
/// type by its `typname`.
fn pgcast(types: &str, casts: &str) -> Result<String, String> {
    let oids = type_oids(types)?;
    let mut preferred = Vec::new();
    for entry in dat_entries("pg_type.dat", types)? {
        if entry.get("typispreferred").map(String::as_str) == Some("t") {
            let name = entry.get("typname").map(String::as_str).unwrap_or_default();
            preferred.push(oids[name]);
        }
    }
    let mut rows = Vec::new();
    for entry in dat_entries("pg_cast.dat", casts)? {
        let oid = |key: &str| {
            let name =
                entry.get(key).ok_or_else(|| format!("pg_cast.dat: a cast with no {key}"))?;
            oids.get(name).copied().ok_or_else(|| format!("pg_cast.dat: no type {name}"))
        };
        let (source, target) = (oid("castsource")?, oid("casttarget")?);
        let letter = |key: &str, letters: &str| match entry.get(key).map(String::as_str) {
            Some(v) if v.len() == 1 && letters.contains(v) => Ok(v.to_string()),
            _ => Err(format!("pg_cast.dat: the cast {source} to {target} has a bad {key}")),
        };
        rows.push((source, target, letter("castcontext", "iae")?, letter("castmethod", "fib")?));
    }
    preferred.sort_unstable();
    rows.sort_unstable();
    if rows.windows(2).any(|w| (w[0].0, w[0].1) == (w[1].0, w[1].1)) {
        return Err("pg_cast.dat: two casts have the same types".to_string());
    }
    let mut out = String::from(
        "//! The preferred types of `pg_type.dat` and the casts of `pg_cast.dat`.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-pgtypes/vendor/pg_type.dat` and\n\
         //! `crates/rudb-pgtypes/vendor/pg_cast.dat`. Do not edit. `cargo xtask pg-check` runs in the\n\
         //! gate and fails if this file and the vendored files disagree.\n\
         \n\
         use crate::coerce::{Cast, c};\n\
         use crate::types::Oid;\n\
         \n",
    );
    let _ = writeln!(
        out,
        "/// The types with `typispreferred`, in the order of the OIDs.\n\
         pub(crate) static PREFERRED: [Oid; {}] = {preferred:?};\n\
         \n\
         /// Every cast in the order of the source and the target: the source, the target,\n\
         /// `castcontext` and `castmethod`.\n\
         pub(crate) static CASTS: [Cast; {}] = [",
        preferred.len(),
        rows.len()
    );
    for (source, target, context, method) in rows {
        let _ = writeln!(out, "    c({source}, {target}, b'{context}', b'{method}'),");
    }
    out.push_str("];\n");
    Ok(out)
}

/// Renders the functions of `pg_proc.dat` in the order of the name and the OID, which is the
/// order in which a call finds the functions of its name. A function takes its input arguments,
/// the arguments of `proargtypes`, and the names of these come from `proargnames`, without the
/// output arguments when `proargmodes` has some. The defaults of `proargdefaults` belong to the
/// last input arguments. A field that an entry leaves out has the default of `pg_proc.h`.
fn pgproc(types: &str, procs: &str, system_functions: &str) -> Result<String, String> {
    let oids = type_oids(types)?;
    let mut bodies = sql_bodies(system_functions, &oids)?;
    let mut rows = Vec::new();
    for entry in dat_entries("pg_proc.dat", procs)? {
        let field = |key: &str| entry.get(key).map(String::as_str);
        let name = field("proname").ok_or("pg_proc.dat: an entry with no proname")?;
        let bad = |what: &str| format!("pg_proc.dat: {name} has a bad {what}");
        let oid: u32 = field("oid").and_then(|v| v.parse().ok()).ok_or_else(|| bad("oid"))?;
        let type_oid = |typname: &str| {
            oids.get(typname).copied().ok_or_else(|| format!("pg_proc.dat: no type {typname}"))
        };
        let args: Vec<u32> = field("proargtypes")
            .unwrap_or_default()
            .split_whitespace()
            .map(type_oid)
            .collect::<Result<_, _>>()?;
        let result = type_oid(field("prorettype").ok_or_else(|| bad("prorettype"))?)?;
        let variadic = match field("provariadic") {
            None | Some("0") => 0,
            Some(typname) => type_oid(typname)?,
        };
        let letter = |key: &str, default: char, letters: &str| match field(key) {
            None => Ok(default),
            Some(v) if v.len() == 1 && letters.contains(v) => {
                Ok(v.chars().next().unwrap_or(default))
            }
            Some(_) => Err(bad(key)),
        };
        let kind = letter("prokind", 'f', "fawp")?;
        let strict = letter("proisstrict", 't', "tf")? == 't';
        let retset = letter("proretset", 'f', "tf")? == 't';
        let volatility = letter("provolatile", 'i', "isv")?;
        let mut names = field("proargnames").map(array_items).unwrap_or_default();
        if let Some(modes) = field("proargmodes").map(array_items) {
            if modes.len() != names.len() && !names.is_empty() {
                return Err(bad("proargnames"));
            }
            names = names
                .into_iter()
                .zip(&modes)
                .filter(|(_, mode)| matches!(mode.as_str(), "i" | "b" | "v"))
                .map(|(name, _)| name)
                .collect();
        }
        if !names.is_empty() && names.len() != args.len() {
            return Err(bad("proargnames"));
        }
        let defaults = field("proargdefaults").map(array_items).unwrap_or_default();
        if defaults.len() > args.len() {
            return Err(bad("proargdefaults"));
        }
        let lang = match field("prolang") {
            None | Some("internal") => 'i',
            Some("c") => 'c',
            Some("sql") => 's',
            Some(_) => return Err(bad("prolang")),
        };
        let mut src = field("prosrc").ok_or_else(|| bad("prosrc"))?.to_string();
        if lang == 's'
            && let Some(body) = bodies.remove(&(name.to_string(), args.clone()))
        {
            src = body;
        }
        rows.push((
            name.to_string(),
            oid,
            args,
            result,
            variadic,
            kind,
            strict,
            retset,
            volatility,
            names,
            defaults,
            lang,
            src,
        ));
    }
    if let Some(((name, _), _)) = bodies.into_iter().next() {
        return Err(format!("system_functions.sql: {name} is not a function of pg_proc.dat"));
    }
    rows.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    if rows.windows(2).any(|w| w[0].1 == w[1].1) {
        return Err("pg_proc.dat: two functions have the same OID".to_string());
    }
    let strings = |items: &[String]| {
        let quoted: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
        format!("&[{}]", quoted.join(", "))
    };
    let mut out = String::from(
        "//! The built-in functions of PostgreSQL, one for each entry of `pg_proc.dat`.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-pgtypes/vendor/pg_type.dat`,\n\
         //! `crates/rudb-pgtypes/vendor/pg_proc.dat` and\n\
         //! `crates/rudb-pgtypes/vendor/system_functions.sql`. Do not edit. `cargo xtask pg-check`\n\
         //! runs in the gate and fails if this file and the vendored files disagree.\n\
         \n\
         use crate::procs::{Proc, p};\n\
         \n",
    );
    let _ = writeln!(
        out,
        "/// Every function in the order of the name and the OID: the OID, `proname`, the input\n\
         /// argument types, `prorettype`, `provariadic`, `prokind`, `proisstrict`, `proretset`,\n\
         /// `provolatile`, the names of the input arguments, `proargdefaults`, `prolang` and\n\
         /// `prosrc`, which is the expression after `RETURN` for a function in SQL that has one.\n\
         pub(crate) static PROCS: [Proc; {}] = [",
        rows.len()
    );
    for (
        name,
        oid,
        args,
        result,
        variadic,
        kind,
        strict,
        retset,
        volatility,
        names,
        defaults,
        lang,
        src,
    ) in &rows
    {
        let _ = writeln!(
            out,
            "    p({oid}, {name:?}, &{args:?}, {result}, {variadic}, b'{kind}', {strict}, {retset}, b'{volatility}', {}, {}, b'{lang}', {src:?}),",
            strings(names),
            strings(defaults)
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// Renders the collations of `pg_collation.dat` in the order of the name. The locale is
/// `colllocale` for the builtin and the ICU providers and `collcollate` for the C library, which
/// keeps `collcollate` and `collctype` the same for each of its built-in collations.
fn pgcollation(collations: &str) -> Result<String, String> {
    let mut rows = Vec::new();
    for entry in dat_entries("pg_collation.dat", collations)? {
        let field = |key: &str| entry.get(key).map(String::as_str);
        let name = field("collname").ok_or("pg_collation.dat: an entry with no collname")?;
        let bad = |what: &str| format!("pg_collation.dat: {name} has a bad {what}");
        let oid: u32 = field("oid").and_then(|v| v.parse().ok()).ok_or_else(|| bad("oid"))?;
        let provider = match field("collprovider") {
            Some(provider @ ("d" | "c" | "b" | "i")) => provider,
            _ => return Err(bad("collprovider")),
        };
        let encoding: i32 = field("collencoding")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| bad("collencoding"))?;
        if field("collcollate") != field("collctype") {
            return Err(bad("collctype"));
        }
        let locale = field("colllocale").or(field("collcollate")).unwrap_or_default();
        rows.push((name.to_string(), oid, provider.to_string(), encoding, locale.to_string()));
    }
    rows.sort();
    if rows.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err("pg_collation.dat: two collations have the same name".to_string());
    }
    let mut out = String::from(
        "//! The built-in collations of PostgreSQL, one for each entry of `pg_collation.dat`.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-pgtypes/vendor/pg_collation.dat`.\n\
         //! Do not edit. `cargo xtask pg-check` runs in the gate and fails if this file and the\n\
         //! vendored file disagree.\n\
         \n\
         use crate::collations::Collation;\n\
         \n",
    );
    let _ = writeln!(
        out,
        "/// Every collation in the order of the name: the OID, `collname`, `collprovider`,\n\
         /// `collencoding` and the locale.\n\
         pub(crate) static COLLATIONS: [Collation; {}] = [",
        rows.len()
    );
    for (name, oid, provider, encoding, locale) in &rows {
        let _ = writeln!(
            out,
            "    Collation {{ oid: {oid}, name: {name:?}, provider: b'{provider}', encoding: {encoding}, locale: {locale:?} }},"
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// Renders the operators of `pg_operator.dat` in the order of the name and the OID, which is the
/// order in which an operator finds the candidates of its name. A prefix operator has the left
/// type 0. `oprcode` names a function of `pg_proc.dat`, and the generator checks that one function
/// of that name takes the types of the operator, which is the function that the operator calls.
fn pgoperator(types: &str, operators: &str, procs: &str) -> Result<String, String> {
    let oids = type_oids(types)?;
    let mut functions: BTreeMap<String, Vec<Vec<u32>>> = BTreeMap::new();
    for entry in dat_entries("pg_proc.dat", procs)? {
        let name = entry.get("proname").ok_or("pg_proc.dat: an entry with no proname")?;
        let args: Vec<u32> = entry
            .get("proargtypes")
            .map(String::as_str)
            .unwrap_or_default()
            .split_whitespace()
            .map(|typname| {
                oids.get(typname).copied().ok_or_else(|| format!("pg_proc.dat: no type {typname}"))
            })
            .collect::<Result<_, _>>()?;
        functions.entry(name.clone()).or_default().push(args);
    }
    let mut rows = Vec::new();
    for entry in dat_entries("pg_operator.dat", operators)? {
        let field = |key: &str| entry.get(key).map(String::as_str);
        let name = field("oprname").ok_or("pg_operator.dat: an entry with no oprname")?;
        let bad = |what: &str| format!("pg_operator.dat: {name} has a bad {what}");
        let oid: u32 = field("oid").and_then(|v| v.parse().ok()).ok_or_else(|| bad("oid"))?;
        let type_oid = |key: &str| match field(key) {
            None | Some("0") => Ok(0),
            Some(typname) => oids
                .get(typname)
                .copied()
                .ok_or_else(|| format!("pg_operator.dat: no type {typname}")),
        };
        let kind = match field("oprkind") {
            None | Some("b") => 'b',
            Some("l") => 'l',
            Some(_) => return Err(bad("oprkind")),
        };
        let (left, right, result) =
            (type_oid("oprleft")?, type_oid("oprright")?, type_oid("oprresult")?);
        if (kind == 'l') != (left == 0) || right == 0 || result == 0 {
            return Err(bad("oprleft, oprright or oprresult"));
        }
        // A regproc of an overloaded name has its argument types after it, which are the types
        // of the operator.
        let code = field("oprcode").ok_or_else(|| bad("oprcode"))?;
        let code = code.split_once('(').map_or(code, |(name, _)| name);
        let operands: Vec<u32> = [left, right].into_iter().filter(|&oid| oid != 0).collect();
        let takes = functions
            .get(code)
            .map_or(0, |all| all.iter().filter(|args| **args == operands).count());
        if takes != 1 {
            return Err(format!(
                "pg_operator.dat: {name} calls {code}, which is not one function of its types"
            ));
        }
        // `oprcanmerge` and `oprcanhash` as the flags `M` and `H` of the table.
        let flags = match (field("oprcanmerge") == Some("t"), field("oprcanhash") == Some("t")) {
            (false, false) => "N",
            (true, false) => "M",
            (false, true) => "H",
            (true, true) => "MH",
        };
        rows.push((name.to_string(), oid, kind, operands, result, code.to_string(), flags));
    }
    rows.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    if rows.windows(2).any(|w| w[0].1 == w[1].1) {
        return Err("pg_operator.dat: two operators have the same OID".to_string());
    }
    let mut out = String::from(
        "//! The built-in operators of PostgreSQL, one for each entry of `pg_operator.dat`.\n\
         //!\n\
         //! @generated by `cargo xtask pg-vendor` from `crates/rudb-pgtypes/vendor/pg_type.dat`,\n\
         //! `crates/rudb-pgtypes/vendor/pg_operator.dat` and\n\
         //! `crates/rudb-pgtypes/vendor/pg_proc.dat`. Do not edit. `cargo xtask pg-check` runs in\n\
         //! the gate and fails if this file and the vendored files disagree.\n\
         \n\
         use crate::operators::{H, M, MH, N, Operator, o};\n\
         \n",
    );
    let _ = writeln!(
        out,
        "/// Every operator in the order of the name and the OID: the OID, `oprname`, `oprkind`, the\n\
         /// types of the operands, `oprresult`, `oprcode`, and the flags of `oprcanmerge` and\n\
         /// `oprcanhash`.\n\
         pub(crate) static OPERATORS: [Operator; {}] = [",
        rows.len()
    );
    for (name, oid, kind, operands, result, code, flags) in &rows {
        let _ = writeln!(
            out,
            "    o({oid}, {name:?}, b'{kind}', &{operands:?}, {result}, {code:?}, {flags}),"
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// The bodies of the functions of `system_functions.sql` that are one `RETURN` expression, keyed
/// by the name and the argument types. A function with a `BEGIN ATOMIC` body is not here.
///
/// The header names the input arguments by type, with an optional mode and name in front and an
/// optional `DEFAULT` after. A type is a `typname` or one of the SQL spellings that the file uses.
fn sql_bodies(
    text: &str,
    oids: &BTreeMap<String, u32>,
) -> Result<BTreeMap<(String, Vec<u32>), String>, String> {
    const HEAD: &str = "CREATE OR REPLACE FUNCTION ";
    let spelled = |words: &str| -> Option<u32> {
        let (base, array) = match words.strip_suffix("[]") {
            Some(base) => (base.trim(), true),
            None => (words, false),
        };
        let typname = match base {
            "integer" => "int4",
            "bigint" => "int8",
            "smallint" => "int2",
            "real" => "float4",
            "double precision" => "float8",
            "boolean" => "bool",
            "character varying" => "varchar",
            "timestamp with time zone" => "timestamptz",
            "timestamp without time zone" => "timestamp",
            "time with time zone" => "timetz",
            other => other,
        };
        let typname = if array { format!("_{typname}") } else { typname.to_string() };
        oids.get(&typname).copied()
    };
    let mut bodies = BTreeMap::new();
    for statement in text.split(HEAD).skip(1) {
        let statement = statement.split("\n\n").next().unwrap_or(statement);
        let Some((_, body)) = statement.split_once("\nRETURN ") else {
            continue;
        };
        let body = body.split_once(';').map(|(body, _)| body).ok_or_else(|| {
            format!("system_functions.sql: a RETURN with no `;` after it: {body}")
        })?;
        let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let (name, rest) = statement.split_once('(').ok_or("system_functions.sql: no `(`")?;
        let name = name.trim().trim_matches('"').to_string();
        let list = rest.split_once(')').map(|(list, _)| list).unwrap_or_default();
        let mut args = Vec::new();
        for arg in list.split(',').map(|arg| arg.split_whitespace().collect::<Vec<_>>()) {
            let words = match arg.first() {
                Some(&"OUT") => continue,
                Some(&"IN" | &"VARIADIC") => &arg[1..],
                _ => &arg[..],
            };
            let end = words.iter().position(|word| *word == "DEFAULT").unwrap_or(words.len());
            let words = &words[..end];
            let oid = spelled(&words.join(" "))
                .or_else(|| spelled(&words.get(1..).unwrap_or_default().join(" ")))
                .ok_or_else(|| format!("system_functions.sql: {name} has an argument {words:?}"))?;
            args.push(oid);
        }
        if bodies.insert((name.clone(), args), body.to_string()).is_some() {
            return Err(format!("system_functions.sql: {name} is there twice"));
        }
    }
    Ok(bodies)
}

/// The items of an array literal of a `.dat` file, such as `{"{}",false}`. An item in double
/// quotes loses them, and a backslash in it keeps the character after it.
fn array_items(text: &str) -> Vec<String> {
    let inner = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')).unwrap_or(text);
    let mut items = Vec::new();
    if inner.is_empty() {
        return items;
    }
    let (mut item, mut quoted) = (String::new(), false);
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => item.extend(chars.next()),
            ',' if !quoted => items.push(std::mem::take(&mut item)),
            c => item.push(c),
        }
    }
    items.push(item);
    items
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
                    { oid => '19', typname => 'name', typlen => 'NAMEDATALEN',\n  typcategory => 'S', typelem => 'char',\n  typcollation => 'C' },\n\
                    { oid => '18', typname => 'char', typlen => '1', typcategory => 'Z' },\n\
                    { oid => '603', array_type_oid => '1020', typname => 'box', typlen => '32',\n  \
                    typcategory => 'G', typdelim => ';' },\n\
                    ]\n";
        let collations = "{ oid => '950', collname => 'C' },";
        let out = pgtype(text, collations).expect("the sample parses");
        assert!(out.contains("/// `bool`, boolean, format 't'/'f'.\npub const BOOL: Oid = 16;\n"));
        assert!(
            out.contains("/// `_bool`, the array of `bool`.\npub const BOOL_ARRAY: Oid = 1000;\n")
        );
        assert!(out.contains("TYPES: [TypeInfo; 6]"));
        assert!(out.contains("    t(16, \"bool\", b'b', b'B', 1, 0, 1000, b',', 0),\n"));
        assert!(out.contains("    t(19, \"name\", b'b', b'S', 64, 18, 0, b',', 950),\n"));
        assert!(out.contains("    t(1020, \"_box\", b'b', b'A', -1, 603, 0, b';', 0),\n"));
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
        // The casts are in the order of the source and the target, with the context and the method.
        assert!(out.contains(
            "CASTS: [Cast; 3] = [\n    c(20, 23, b'a', b'f'),\n    c(23, 20, b'i', b'f'),\n    \
             c(23, 701, b'i', b'f'),\n];"
        ));
        assert!(
            pgcast(types, "{ castsource => 'int4', casttarget => 'x', castcontext => 'i' }")
                .is_err()
        );
    }

    #[test]
    fn a_type_with_an_unknown_element_or_a_second_oid_is_refused() {
        let elem =
            "{ oid => '1', typname => 'a', typlen => '1', typcategory => 'A', typelem => 'b' }";
        assert!(pgtype(elem, "").is_err());
        let twice = "{ oid => '1', typname => 'a', typlen => '1', typcategory => 'A' },\n\
                     { oid => '1', typname => 'b', typlen => '1', typcategory => 'A' }";
        assert!(pgtype(twice, "").is_err());
        assert!(pgtype("{ oid => '1', typname => 'a', typlen => '1' }", "").is_err());
        let collated = "{ oid => '1', typname => 'a', typlen => '1', typcategory => 'S',\n  \
                        typcollation => 'x' }";
        assert!(pgtype(collated, "").is_err());
    }
}
