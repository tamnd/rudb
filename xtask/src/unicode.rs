//! Writing the grapheme break table the string kernels read out of the pin's copy of utf8proc.
//!
//! `reverse`, `length_grapheme` and the other grapheme functions split a string where utf8proc
//! says one extended grapheme cluster ends and the next begins, and that depends on two properties
//! of every code point: its boundclass and its indic conjunct break value. Both come from the
//! Unicode version utf8proc was built against, which is 15.1 for the copy duckdb vendors, and a
//! table taken from any other version would split a handful of new emoji and Indic conjuncts
//! differently. So the table is read out of `third_party/utf8proc/utf8proc_data.cpp` in a duckdb
//! checkout at the pin rather than out of the Unicode character database, which makes it the pin's
//! table by construction rather than by agreement.
//!
//! The source is two megabytes and is not vendored. The generated file records the commit and the
//! checksum of the file it was read from instead, and `--check` regenerates from a checkout and
//! fails if the committed table has drifted from it.

use std::path::Path;

use crate::sha256;

/// Where the table goes, relative to the workspace root.
const DEST: &str = "crates/rudb-kernels/src/graphemes/table.rs";
/// The duckdb commit the pinned binary was built from, which the source has to come from.
const COMMIT: &str = "cc7e7bac7f";

/// The boundclass names in the order of `utf8proc_boundclass_t`, so a name's index is its value.
const BOUNDCLASSES: [&str; 21] = [
    "START",
    "OTHER",
    "CR",
    "LF",
    "CONTROL",
    "EXTEND",
    "L",
    "V",
    "T",
    "LV",
    "LVT",
    "REGIONAL_INDICATOR",
    "SPACINGMARK",
    "PREPEND",
    "ZWJ",
    "E_BASE",
    "E_MODIFIER",
    "GLUE_AFTER_ZWJ",
    "E_BASE_GAZ",
    "EXTENDED_PICTOGRAPHIC",
    "E_ZWG",
];
/// The indic conjunct break names in the order of `utf8proc_indic_conjunct_break_t`.
const CONJUNCT_BREAKS: [&str; 4] = ["NONE", "LINKER", "CONSONANT", "EXTEND"];

/// Reads `source` and writes the table, or with `check` compares it with the one committed.
pub(crate) fn generate(source: Option<&str>, check: bool) -> Result<(), String> {
    let Some(source) = source else {
        return Err(format!(
            "usage: cargo xtask gen-unicode <duckdb checkout at {COMMIT}>/third_party/utf8proc/utf8proc_data.cpp [--check]"
        ));
    };
    let bytes = std::fs::read(source).map_err(|e| format!("could not read {source}: {e}"))?;
    let text = String::from_utf8(bytes.clone()).map_err(|e| format!("{source}: {e}"))?;
    let runs = runs(&text)?;
    let written = emit(&runs, &sha256::hex(&bytes));
    let dest = crate::root().join(DEST);
    if check {
        let committed = std::fs::read_to_string(&dest).unwrap_or_default();
        if committed != written {
            return Err(format!("{DEST} is not what {source} generates"));
        }
        println!("{DEST} matches {source}");
        return Ok(());
    }
    write(&dest, &written)?;
    println!("wrote {} runs to {DEST}", runs.len());
    Ok(())
}

fn write(dest: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
    }
    std::fs::write(dest, text).map_err(|e| format!("could not write {}: {e}", dest.display()))
}

/// Every run of code points that share a class, as its first code point and the packed class.
///
/// The class is the boundclass in the low five bits and the indic conjunct break value in the two
/// above them. The lookup utf8proc does is two stage, `stage2[stage1[cp >> 8] + (cp & 0xFF)]`
/// into the property array, and this walks every code point through it once.
fn runs(text: &str) -> Result<Vec<(u32, u8)>, String> {
    let stage1 = numbers(array(text, "utf8proc_stage1table")?)?;
    let stage2 = numbers(array(text, "utf8proc_stage2table")?)?;
    let classes = properties(array(text, "utf8proc_properties")?)?;
    let mut runs: Vec<(u32, u8)> = Vec::new();
    for cp in 0..0x11_0000u32 {
        let first = *stage1.get((cp >> 8) as usize).ok_or("stage1 table is short")?;
        let second = *stage2.get(first as usize + (cp & 0xFF) as usize).ok_or("stage2 is short")?;
        let class = *classes.get(second as usize).ok_or("a property index is out of range")?;
        if runs.last().is_none_or(|&(_, last)| last != class) {
            runs.push((cp, class));
        }
    }
    Ok(runs)
}

/// The text between the braces of the array called `name`.
fn array<'a>(text: &'a str, name: &str) -> Result<&'a str, String> {
    let marker = format!("{name}[] = {{");
    let start = text.find(&marker).ok_or(format!("no {name} in the source"))? + marker.len();
    let length = text[start..].find("};").ok_or(format!("{name} is not closed"))?;
    Ok(&text[start..start + length])
}

fn numbers(body: &str) -> Result<Vec<u32>, String> {
    body.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.parse().map_err(|e| format!("{item}: {e}")))
        .collect()
}

/// The packed class of every entry in the property array, in order.
///
/// Each entry is a brace group whose last two fields are the boundclass and the indic conjunct
/// break value, spelled as enum names except in the first entry, which is all zeros and ones.
fn properties(body: &str) -> Result<Vec<u8>, String> {
    let mut classes = Vec::new();
    for group in body.split('{').skip(1) {
        let fields = group.split('}').next().unwrap_or_default();
        let fields: Vec<&str> = fields.split(',').map(str::trim).collect();
        let [.., bound, conjunct] = fields[..] else {
            return Err(format!("a property entry with too few fields: {group}"));
        };
        let bound = named(bound, "UTF8PROC_BOUNDCLASS_", &BOUNDCLASSES)?;
        let conjunct = named(conjunct, "UTF8PROC_INDIC_CONJUNCT_BREAK_", &CONJUNCT_BREAKS)?;
        classes.push(bound | conjunct << 5);
    }
    Ok(classes)
}

fn named(field: &str, prefix: &str, names: &[&str]) -> Result<u8, String> {
    let name = field.strip_prefix(prefix).ok_or(format!("{field} is not a {prefix} name"))?;
    let at = names.iter().position(|known| *known == name).ok_or(format!("unknown {field}"))?;
    Ok(at as u8)
}

fn emit(runs: &[(u32, u8)], checksum: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "//! The grapheme break class of every code point, as the pin's copy of utf8proc has it.\n",
    );
    out.push_str("//!\n");
    out.push_str(&format!(
        "//! @generated by `cargo xtask gen-unicode` from `third_party/utf8proc/utf8proc_data.cpp` at\n\
         //! duckdb {COMMIT}, whose sha256 is\n//! {checksum}.\n//! Do not edit.\n"
    ));
    out.push_str("//!\n");
    out.push_str(
        "//! One row for every run of code points that share a class: the first code point of the \
         run\n//! and the class, which is the boundclass in the low five bits and the indic \
         conjunct break\n//! value in the two above them. A code point's class is in the last \
         row that starts at or\n//! before it.\n\n",
    );
    out.push_str(&format!(
        "#[rustfmt::skip]\npub(super) static RUNS: [(u32, u8); {}] = [\n",
        runs.len()
    ));
    for row in runs.chunks(6) {
        let cells: Vec<String> =
            row.iter().map(|(start, class)| format!("(0x{start:05X}, {class})")).collect();
        out.push_str(&format!("    {},\n", cells.join(", ")));
    }
    out.push_str("];\n");
    out
}
