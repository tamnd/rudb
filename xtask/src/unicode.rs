//! Writing the grapheme break table and the normalization tables the string kernels read out of
//! the pin's copy of utf8proc.
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
//! `nfc_normalize` and `strip_accents` are utf8proc's own decompose and compose loops, and they
//! read four more properties: the combining class, whether the code point is a mark, its canonical
//! decomposition one level deep, and the pairs that compose. Those are read out of the same file
//! for the same reason, and the pairs are read the way utf8proc's compose loop reads its packed
//! combination array, so a pair is in the table exactly when the pin would compose it.
//!
//! The source is two megabytes and is not vendored. The generated file records the commit and the
//! checksum of the file it was read from instead, and `--check` regenerates from a checkout and
//! fails if the committed table has drifted from it.

use std::path::Path;

use crate::sha256;

/// Where the table goes, relative to the workspace root.
const DEST: &str = "crates/rudb-kernels/src/graphemes/table.rs";
/// Where the normalization tables go.
const NORMALIZE_DEST: &str = "crates/rudb-kernels/src/normalize/table.rs";
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
    let checksum = sha256::hex(&bytes);
    let runs = runs(&text)?;
    let tables = normalization(&text)?;
    let outputs =
        [(DEST, emit(&runs, &checksum)), (NORMALIZE_DEST, emit_normal(&tables, &checksum))];
    for (dest, written) in outputs {
        let path = crate::root().join(dest);
        if check {
            let committed = std::fs::read_to_string(&path).unwrap_or_default();
            if committed != written {
                return Err(format!("{dest} is not what {source} generates"));
            }
            println!("{dest} matches {source}");
        } else {
            write(&path, &written)?;
            println!("wrote {dest}");
        }
    }
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

/// What the normalization kernels read: the combining class and mark runs, the canonical
/// decompositions one level deep, and the composing pairs.
struct Normal {
    classes: Vec<(u32, u8, bool)>,
    decompositions: Vec<(u32, u32, u32)>,
    compositions: Vec<(u32, u32, u32)>,
}

/// The fields of one property entry that normalization reads.
struct Property {
    class: u8,
    mark: bool,
    decomposition: Option<u16>,
    comb: u16,
    excluded: bool,
}

fn normalization(text: &str) -> Result<Normal, String> {
    let stage1 = numbers(array(text, "utf8proc_stage1table")?)?;
    let stage2 = numbers(array(text, "utf8proc_stage2table")?)?;
    let sequences = numbers(array(text, "utf8proc_sequences")?)?;
    let combinations = numbers(array(text, "utf8proc_combinations")?)?;
    let table = normal_properties(array(text, "utf8proc_properties")?)?;
    let property = |cp: u32| -> Result<&Property, String> {
        let first = *stage1.get((cp >> 8) as usize).ok_or("stage1 table is short")?;
        let second = *stage2.get(first as usize + (cp & 0xFF) as usize).ok_or("stage2 is short")?;
        table.get(second as usize).ok_or_else(|| "a property index is out of range".to_string())
    };
    let (mut classes, mut decompositions) = (Vec::new(), Vec::new());
    let (mut starters, mut combiners) = (Vec::new(), Vec::new());
    for cp in 0..0x11_0000u32 {
        let found = property(cp)?;
        if classes.last().is_none_or(|&(_, class, mark)| (class, mark) != (found.class, found.mark))
        {
            classes.push((cp, found.class, found.mark));
        }
        if let Some(index) = found.decomposition {
            let pieces = sequence(&sequences, index)?;
            match pieces[..] {
                [only] => decompositions.push((cp, only, 0)),
                [first, second] => decompositions.push((cp, first, second)),
                _ => {
                    return Err(format!("U+{cp:04X} decomposes into {} code points", pieces.len()));
                }
            }
        }
        if found.comb < 0x8000 {
            starters.push((cp, found.comb));
        } else if found.comb != u16::MAX {
            combiners.push((cp, found.comb));
        }
    }
    // The compose loop in utf8proc_normalize_utf32, run over every starter and combiner pair.
    let at = |index: usize| -> Result<u32, String> {
        combinations.get(index).copied().ok_or_else(|| "the combinations are short".to_string())
    };
    let mut compositions = Vec::new();
    for &(starter, sidx) in &starters {
        let sidx = sidx as usize;
        let (low, high) = (at(sidx)?, at(sidx + 1)?);
        for &(combiner, comb) in &combiners {
            let index = u32::from(comb & 0x3FFF);
            if index < low || index > high {
                continue;
            }
            let index = (index + sidx as u32 + 2 - low) as usize;
            let composed =
                if comb & 0x4000 != 0 { at(index)? << 16 | at(index + 1)? } else { at(index)? };
            if composed > 0 && !property(composed)?.excluded {
                compositions.push((starter, combiner, composed));
            }
        }
    }
    compositions.sort_unstable();
    Ok(Normal { classes, decompositions, compositions })
}

/// The code points of the sequence at `index`, which is utf8proc's `seqindex_write_char_decomposed`
/// with the recursion left to the kernel: the length is in the top two bits, or in the first entry
/// when it is three or more, and an entry can be a surrogate pair.
fn sequence(sequences: &[u32], index: u16) -> Result<Vec<u32>, String> {
    let short = || "the sequences are short".to_string();
    let mut at = usize::from(index & 0x3FFF);
    let mut left = i32::from(index >> 14);
    if left >= 3 {
        left = *sequences.get(at).ok_or_else(short)? as i32;
        at += 1;
    }
    let mut pieces = Vec::new();
    while left >= 0 {
        let mut cp = *sequences.get(at).ok_or_else(short)?;
        if cp & 0xF800 == 0xD800 {
            at += 1;
            cp = ((cp & 0x03FF) << 10 | (*sequences.get(at).ok_or_else(short)? & 0x03FF)) + 0x10000;
        }
        pieces.push(cp);
        at += 1;
        left -= 1;
    }
    Ok(pieces)
}

/// The normalization fields of every entry in the property array, in order.
///
/// The fields are, by position, the category, the combining class, the bidi class, the
/// decomposition type, the decomposition index, four case indices, the combination index and then
/// the flags, the second of which is the composition exclusion.
fn normal_properties(body: &str) -> Result<Vec<Property>, String> {
    let index = |field: &str| -> Result<u16, String> {
        if field == "UINT16_MAX" {
            Ok(u16::MAX)
        } else {
            field.parse().map_err(|e| format!("{field}: {e}"))
        }
    };
    let mut table = Vec::new();
    for group in body.split('{').skip(1) {
        let fields = group.split('}').next().unwrap_or_default();
        let fields: Vec<&str> = fields.split(',').map(str::trim).collect();
        let [category, class, _, kind, decomposition, _, _, _, _, comb, _, excluded, ..] =
            fields[..]
        else {
            return Err(format!("a property entry with too few fields: {group}"));
        };
        let mark = matches!(
            category,
            "UTF8PROC_CATEGORY_MN" | "UTF8PROC_CATEGORY_MC" | "UTF8PROC_CATEGORY_ME"
        );
        let class = class.parse().map_err(|e| format!("{class}: {e}"))?;
        // Only a canonical decomposition, which is one with no type, is read without COMPAT.
        let decomposition = Some(index(decomposition)?).filter(|&at| at != u16::MAX && kind == "0");
        let comb = index(comb)?;
        let excluded = match excluded {
            "true" => true,
            "false" => false,
            other => return Err(format!("{other} is not a flag")),
        };
        table.push(Property { class, mark, decomposition, comb, excluded });
    }
    Ok(table)
}

fn emit_normal(tables: &Normal, checksum: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "//! The normalization properties of every code point, as the pin's copy of utf8proc has \
         them.\n",
    );
    out.push_str("//!\n");
    out.push_str(&format!(
        "//! @generated by `cargo xtask gen-unicode` from `third_party/utf8proc/utf8proc_data.cpp` at\n\
         //! duckdb {COMMIT}, whose sha256 is\n//! {checksum}.\n//! Do not edit.\n"
    ));
    out.push_str("//!\n");
    out.push_str(
        "//! `CLASSES` has one row for every run of code points that share a combining class and \
         a\n//! mark flag, and a code point's row is the last one that starts at or before it. \
         `DECOMPOSITIONS`\n//! is each canonical decomposition one level deep, with a zero second \
         half for one of one code\n//! point. `COMPOSITIONS` is every starter and combiner pair \
         the compose loop joins, and what it\n//! joins them into, with the pairs whose result is \
         a composition exclusion left out.\n\n",
    );
    out.push_str(&format!(
        "#[rustfmt::skip]\npub(super) static CLASSES: [(u32, u8, bool); {}] = [\n",
        tables.classes.len()
    ));
    for row in tables.classes.chunks(4) {
        let cells: Vec<String> = row
            .iter()
            .map(|(start, class, mark)| format!("(0x{start:05X}, {class}, {mark})"))
            .collect();
        out.push_str(&format!("    {},\n", cells.join(", ")));
    }
    out.push_str("];\n\n");
    for (name, rows) in
        [("DECOMPOSITIONS", &tables.decompositions), ("COMPOSITIONS", &tables.compositions)]
    {
        out.push_str(&format!(
            "#[rustfmt::skip]\npub(super) static {name}: [(u32, u32, u32); {}] = [\n",
            rows.len()
        ));
        for row in rows.chunks(3) {
            let cells: Vec<String> =
                row.iter().map(|(a, b, c)| format!("(0x{a:05X}, 0x{b:05X}, 0x{c:05X})")).collect();
            out.push_str(&format!("    {},\n", cells.join(", ")));
        }
        out.push_str("];\n\n");
    }
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
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
