//! Turning the vendored grammar into Rust that is checked in.
//!
//! `cargo xtask gen-grammar` writes, `cargo xtask gen-grammar --check` regenerates into memory and
//! fails on any difference. The second one runs in the gate, which is what stops the generated
//! files from being hand edited and then quietly disagreeing with the grammar they claim to come
//! from. Between the two of them, and `cargo xtask grammar` on the vendored tree, the path from
//! upstream's bytes to ours has no step where a person can intervene without the build noticing.
//!
//! The output is checked in rather than written by a build script. Somebody with no network
//! builds rudb, and a build script that reads forty files also runs on every rebuild for the
//! benefit of nobody. It is the same rule the vendored tree lives under.
//!
//! Today this emits the keyword table. `spec/20-the-grammar.md` section 5 has the rest, being the
//! rule table and the first token filter, and that needs the token keys the tokenizer defines, so
//! it lands after the tokenizer rather than before it.

use std::collections::BTreeMap;
use std::path::Path;

/// Where the generated files go. One directory so that `@generated` is a property of a path
/// rather than a thing you have to check per file.
const DEST: &str = "crates/rudb-parse/src/generated";

/// The five keyword classes, in the order their bits are assigned. The names are upstream's file
/// names without the suffix, because the mapping from a `.list` file to a class should be
/// something a reader can check by looking rather than something they have to trust.
const CLASSES: [(&str, &str); 5] = [
    ("reserved_keyword", "RESERVED"),
    ("unreserved_keyword", "UNRESERVED"),
    ("column_name_keyword", "COLUMN_NAME"),
    ("func_name_keyword", "FUNC_NAME"),
    ("type_name_keyword", "TYPE_NAME"),
];

pub(crate) fn generate(check: bool) -> Result<(), String> {
    let root = crate::root();
    let grammar = root.join(crate::vendor::DEST);
    let dest = root.join(DEST);

    let keywords = keyword_table(&grammar)?;
    let written = emit_keywords(&keywords);

    let path = dest.join("keywords.rs");
    if check {
        let found = std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if found.replace("\r\n", "\n") != written {
            return Err(format!(
                "{DEST}/keywords.rs is not what the grammar generates\n  \
                 run `cargo xtask gen-grammar` and commit the result\n  \
                 it is generated from the vendored grammar and editing it by hand puts the \
                 parser and the dialect it claims to implement out of step"
            ));
        }
        println!("the generated keyword table matches the grammar, {} words", keywords.len());
        return Ok(());
    }

    std::fs::create_dir_all(&dest)
        .map_err(|e| format!("could not make {}: {e}", dest.display()))?;
    std::fs::write(&path, &written)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    println!("wrote {DEST}/keywords.rs, {} words", keywords.len());
    Ok(())
}

/// Every word the parser has to recognize, with the classes it belongs to.
///
/// Two sources, and the difference between them is the point. The five `.list` files say which
/// words are keywords and in what class, which is what decides whether a word can be a bare name
/// in a position. The grammar's own quoted literals say which words a rule can spell. Those sets
/// are not the same and neither contains the other.
///
/// 15 words are spelled by a rule and are in no list. They are soft words: `ORDER BY x ASCENDING`
/// parses because a rule spells the literal, and `SELECT ascending FROM t` is still a column
/// reference because the word is in no class and so never blocks an identifier. A generator that
/// took the lists as the authority on what a keyword is would reserve all 15 of them, and nothing
/// would catch it except a user with a column called `prefix`. So a mask of zero is a real state
/// and not a missing entry. `spec/20-the-grammar.md` section 5.
fn keyword_table(grammar: &Path) -> Result<Vec<(String, u8)>, String> {
    let mut table: BTreeMap<String, u8> = BTreeMap::new();

    for (index, (file, _)) in CLASSES.iter().enumerate() {
        let path = grammar.join("keywords").join(format!("{file}.list"));
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        let mut seen = 0usize;
        for word in text.lines() {
            let word = word.trim();
            if word.is_empty() {
                continue;
            }
            check_word(word, &path)?;
            *table.entry(word.to_ascii_lowercase()).or_insert(0) |= 1 << index;
            seen += 1;
        }
        if seen == 0 {
            return Err(format!("{} has no words in it", path.display()));
        }
    }

    for word in grammar_literals(&grammar.join("statements"))? {
        table.entry(word).or_insert(0);
    }

    Ok(table.into_iter().collect())
}

/// Every word the grammar spells as a quoted literal, lower cased.
///
/// A literal is a keyword literal when it is entirely alphabetic. Everything else is punctuation
/// or an operator and belongs to the tokenizer rather than to this table. The split is clean in
/// the grammar as it stands: 330 of the 389 distinct literals are alphabetic and none of the
/// remainder has a letter in it.
fn grammar_literals(statements: &Path) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let entries = std::fs::read_dir(statements)
        .map_err(|e| format!("could not read {}: {e}", statements.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("gram") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        for line in text.lines() {
            // Comments can contain an apostrophe and a rule cannot, so dropping them first is
            // cheaper than teaching the scan below what a comment is.
            let line = line.split('#').next().unwrap_or("");
            let bytes = line.as_bytes();
            let mut at = 0;
            while at < bytes.len() {
                if bytes[at] != b'\'' {
                    at += 1;
                    continue;
                }
                // `'\''` is the only escape the grammar uses and it is not a keyword literal, so
                // stepping over the backslash is enough to keep the scan in step with the quotes.
                let start = at + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'\'' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                if end > bytes.len() {
                    at = bytes.len();
                    continue;
                }
                let body = &line[start..end.min(bytes.len())];
                if !body.is_empty() && body.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') {
                    words.push(body.to_ascii_lowercase());
                }
                at = end + 1;
            }
        }
    }
    Ok(words)
}

/// A keyword list entry has to be a word this table can look up.
///
/// The table is ASCII lower cased and searched by folding the candidate the same way, so a
/// non-ASCII byte in a list would produce an entry nothing can ever match. That has never
/// happened and it would be invisible if it did, which is the case worth an error.
fn check_word(word: &str, path: &Path) -> Result<(), String> {
    if word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Ok(());
    }
    Err(format!("{} has a keyword that is not a plain ASCII word: {word:?}", path.display()))
}

fn emit_keywords(table: &[(String, u8)]) -> String {
    let longest = table.iter().map(|(word, _)| word.len()).max().unwrap_or(0);
    let mut out = String::with_capacity(table.len() * 40);

    out.push_str(
        "//! Every word DuckDB's grammar knows about, and which classes each one is in.\n\
         //!\n\
         //! @generated by `cargo xtask gen-grammar` from the vendored grammar. Do not edit.\n\
         //! `cargo xtask gen-grammar --check` runs in the gate and fails if this file and the\n\
         //! grammar disagree.\n\
         //!\n\
         //! Sorted and lower cased, so a lookup is one binary search with the candidate folded\n\
         //! the same way. One table rather than five, because the classes are not disjoint and\n\
         //! five tables would store the overlapping words more than once and then have to decide\n\
         //! which answer wins. A mask of zero means the word is spelled by some rule and is in no\n\
         //! class, which makes it matchable as a literal and transparent everywhere else.\n\n",
    );

    for (index, (_, name)) in CLASSES.iter().enumerate() {
        out.push_str(&format!(
            "/// The `{}` class.\npub const {name}: u8 = 1 << {index};\n",
            CLASSES[index].0
        ));
    }

    out.push_str(&format!(
        "\n/// The longest keyword, so that folding a candidate can use a fixed buffer and a longer\n\
         /// word can skip the lookup without touching the table at all.\n\
         pub const LONGEST: usize = {longest};\n\n\
         /// The words, sorted by the lower cased spelling.\n\
         pub static KEYWORDS: [(&str, u8); {}] = [\n",
        table.len()
    ));

    for (word, mask) in table {
        let mut classes: Vec<&str> = Vec::new();
        for (index, (_, name)) in CLASSES.iter().enumerate() {
            if mask & (1 << index) != 0 {
                classes.push(name);
            }
        }
        let mask = if classes.is_empty() { "0".to_string() } else { classes.join(" | ") };
        out.push_str(&format!("    (\"{word}\", {mask}),\n"));
    }
    out.push_str("];\n");
    out
}

#[cfg(test)]
mod tests {
    use super::{grammar_literals, keyword_table};

    /// The vendored grammar is in the tree and the gate already checks it is upstream's, so the
    /// generator is tested against the real thing rather than against a fixture that agrees with
    /// whatever the generator happens to do.
    fn grammar() -> std::path::PathBuf {
        crate::root().join(crate::vendor::DEST)
    }

    #[test]
    fn the_soft_words_get_a_mask_of_zero() {
        let table = keyword_table(&grammar()).expect("the vendored grammar does not read");
        let by_word: std::collections::HashMap<&str, u8> =
            table.iter().map(|(word, mask)| (word.as_str(), *mask)).collect();

        // Spelled by a rule, in none of the five lists. `ORDER BY x ASCENDING` parses and
        // `SELECT ascending FROM t` is a column reference, and both of those need the mask to be
        // zero rather than the word to be absent.
        for soft in ["ascending", "descending", "try", "prefix", "keys", "variant"] {
            assert_eq!(by_word.get(soft), Some(&0), "{soft} should be in the table with no class");
        }
        // In a list, so it blocks an identifier in the positions that class excludes.
        assert_ne!(by_word.get("select"), Some(&0));
        assert_ne!(by_word.get("from"), Some(&0));
    }

    #[test]
    fn the_table_is_sorted_and_folded_and_has_no_duplicates() {
        let table = keyword_table(&grammar()).expect("the vendored grammar does not read");
        assert!(table.len() > 400, "only {} words, something did not read", table.len());
        for pair in table.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{:?} then {:?} is not sorted", pair[0].0, pair[1].0);
        }
        for (word, _) in &table {
            assert_eq!(*word, word.to_ascii_lowercase(), "{word} is not folded");
        }
    }

    #[test]
    fn a_word_in_two_classes_keeps_both_bits() {
        let table = keyword_table(&grammar()).expect("the vendored grammar does not read");
        let overlapping = table.iter().filter(|(_, mask)| mask.count_ones() > 1).count();
        // 29 at v2.0. Asserting the exact number would make this test a thing to update on every
        // bump for no reason, but zero would mean the masks are being overwritten rather than
        // combined, which is a bug that would otherwise show up as a valid query being rejected.
        assert!(overlapping > 0, "no word is in two classes, the masks are being overwritten");
    }

    #[test]
    fn the_literal_scan_finds_words_and_skips_operators() {
        let words = grammar_literals(&grammar().join("statements"))
            .expect("the vendored statements do not read");
        assert!(words.contains(&"select".to_string()));
        assert!(words.contains(&"ascending".to_string()));
        // `'&&'`, `'->>'` and the rest are the tokenizer's business and have no place in a
        // keyword table. Nothing with a non-letter in it should come out of here.
        for word in &words {
            assert!(
                word.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{word} is not a keyword literal"
            );
        }
    }
}
