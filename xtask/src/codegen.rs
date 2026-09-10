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
//! Two files come out of here. `keywords.rs` is every word the grammar knows and which classes it
//! is in. `rules.rs` is the grammar itself, compiled to a flat node table with a FIRST set beside
//! every node. They are generated in one run from one read of the vendored tree, which is what
//! lets a `Keyword` node in the rule table carry a bare index into the keyword table.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::ruletable::{self, Op, Table};

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
    let parsed = crate::grammar::parse_dir(&grammar.join("statements"))?;
    let table =
        ruletable::compile(&parsed, &keywords, &overrides(&grammar)?, &memoized(&grammar)?)?;

    let files = [("keywords.rs", emit_keywords(&keywords)), ("rules.rs", emit_rules(&table))];

    if check {
        for (name, written) in &files {
            let path = dest.join(name);
            let found = std::fs::read_to_string(&path)
                .map_err(|e| format!("could not read {}: {e}", path.display()))?;
            if found.replace("\r\n", "\n") != *written {
                return Err(format!(
                    "{DEST}/{name} is not what the grammar generates\n  \
                     run `cargo xtask gen-grammar` and commit the result\n  \
                     it is generated from the vendored grammar and editing it by hand puts the \
                     parser and the dialect it claims to implement out of step"
                ));
            }
        }
        println!(
            "the generated tables match the grammar, {} words and {} rules in {} nodes",
            keywords.len(),
            table.rules.len(),
            table.nodes.len()
        );
        return Ok(());
    }

    std::fs::create_dir_all(&dest)
        .map_err(|e| format!("could not make {}: {e}", dest.display()))?;
    for (name, written) in &files {
        let path = dest.join(name);
        std::fs::write(&path, written)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    }
    println!(
        "wrote {DEST}/keywords.rs with {} words and {DEST}/rules.rs with {} rules in {} nodes",
        keywords.len(),
        table.rules.len(),
        table.nodes.len()
    );
    Ok(())
}

/// The rules the matcher does not walk, from the vendored list.
///
/// Three tab separated fields, and the third is empty for the three matchers that take no
/// suggestion. Split with a limit rather than by filtering empties, so a line that has grown a
/// field is an error here rather than a silently shifted column.
pub(crate) fn overrides(grammar: &Path) -> Result<Vec<(String, String, String)>, String> {
    let path = grammar.join("matcher_overrides.list");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let (name, class, suggestion) = match fields.as_slice() {
            [name, class] => (*name, *class, ""),
            [name, class, suggestion] => (*name, *class, *suggestion),
            _ => {
                return Err(format!(
                    "{}:{}: expected a rule, a matcher class and a suggestion separated by tabs",
                    path.display(),
                    number + 1
                ));
            }
        };
        out.push((name.to_string(), class.to_string(), suggestion.to_string()));
    }
    if out.is_empty() {
        return Err(format!("{} lists no overrides", path.display()));
    }
    Ok(out)
}

/// The rules upstream memoizes, from the vendored list.
pub(crate) fn memoized(grammar: &Path) -> Result<BTreeSet<String>, String> {
    let path = grammar.join("memoized_rules.list");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let out: BTreeSet<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    if out.is_empty() {
        return Err(format!("{} lists no rules", path.display()));
    }
    Ok(out)
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
pub(crate) fn keyword_table(grammar: &Path) -> Result<Vec<(String, u8)>, String> {
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

fn emit_rules(table: &Table) -> String {
    let mut out = String::with_capacity(table.nodes.len() * 64);

    out.push_str(
        "//! DuckDB's grammar, compiled to a table the matcher walks.\n\
         //!\n\
         //! @generated by `cargo xtask gen-grammar` from the vendored grammar. Do not edit.\n\
         //! `cargo xtask gen-grammar --check` runs in the gate and fails if this file and the\n\
         //! grammar disagree.\n\
         //!\n\
         //! `NODES` and `FIRST` are parallel. A node is twelve bytes and its FIRST set is eight\n\
         //! bytes at the same index, so deciding whether an alternative can match the token in\n\
         //! hand reads only `FIRST` and never loads the node. `CHILDREN` holds the child lists of\n\
         //! sequences and choices, contiguous per node, so a sequence is a slice rather than a\n\
         //! chase. Rules are the only nodes that are not expanded in place, which is what keeps\n\
         //! the table finite in the face of a recursive grammar.\n\
         //!\n\
         //! Every table carries `#[rustfmt::skip]`, because the generator and rustfmt disagree\n\
         //! about `NULLABLE`, `CHILDREN` and `SYMBOLS` and something has to win. rustfmt packs an\n\
         //! array whose elements are short onto as many per line as fit, so `cargo fmt` rewrote\n\
         //! this file and then `gen-grammar --check` failed on it, with both halves of the gate\n\
         //! correct and the working tree unable to satisfy them at once. One element a line is the\n\
         //! better answer anyway: a packed array reflows a whole block of lines when one entry\n\
         //! changes, and the point of checking this file in is that a grammar bump is a diff\n\
         //! somebody reads. The attribute is on all six rather than the three, so that a change to\n\
         //! rustfmt's width threshold cannot bring the disagreement back.\n\n\
         use crate::rules::{Node, Op, Rule, Suggestion};\n\n",
    );

    out.push_str(&format!(
        "/// The root. `Program` is what a whole script parses as.\n\
         pub const PROGRAM: u32 = {};\n\n\
         /// The other root upstream requires, for parsing one statement rather than a script.\n\
         pub const TOP_LEVEL_STATEMENT: u32 = {};\n\n",
        table.program, table.top_level
    ));

    out.push_str(&format!(
        "/// The nodes.\n#[rustfmt::skip]\n\
         pub static NODES: [Node; {}] = [\n",
        table.nodes.len()
    ));
    for node in &table.nodes {
        let argument = match node.op {
            Op::Identifier => {
                format!("Suggestion::{} as u32", ruletable::suggestion_name(node.a))
            }
            _ => node.a.to_string(),
        };
        out.push_str(&format!(
            "    Node {{ op: Op::{}, flags: {}, a: {argument}, b: {} }},\n",
            ruletable::op_name(node.op),
            node.flags,
            node.b
        ));
    }
    out.push_str("];\n\n");

    out.push_str(&format!(
        "/// What each node can start with, as a set of token keys. A superset, always: a bit that\n\
         /// is set may still fail to match, and a bit that is clear cannot possibly match, which\n\
         /// is the only direction a filter is allowed to be wrong in.\n\
         #[rustfmt::skip]\n\
         pub static FIRST: [u64; {}] = [\n",
        table.first.len()
    ));
    for set in &table.first {
        out.push_str(&format!("    0x{set:016x},\n"));
    }
    out.push_str("];\n\n");

    out.push_str(&format!(
        "/// Whether each node can match without consuming a token.\n\
         #[rustfmt::skip]\n\
         pub static NULLABLE: [bool; {}] = [\n",
        table.nullable.len()
    ));
    for value in &table.nullable {
        out.push_str(&format!("    {value},\n"));
    }
    out.push_str("];\n\n");

    out.push_str(&format!(
        "/// The child lists of every sequence and choice, contiguous per node.\n\
         #[rustfmt::skip]\n\
         pub static CHILDREN: [u32; {}] = [\n",
        table.children.len()
    ));
    for child in &table.children {
        out.push_str(&format!("    {child},\n"));
    }
    out.push_str("];\n\n");

    out.push_str(&format!(
        "/// The rules, sorted by name so a lookup by name is a binary search and the table does\n\
         /// not depend on the order the generator happened to walk the grammar.\n\
         #[rustfmt::skip]\n\
         pub static RULES: [Rule; {}] = [\n",
        table.rules.len()
    ));
    for rule in &table.rules {
        out.push_str(&format!(
            "    Rule {{ name: \"{}\", root: {}, memoized: {} }},\n",
            rule.name, rule.root, rule.memoized
        ));
    }
    out.push_str("];\n\n");

    out.push_str(&format!(
        "/// Every literal that is not a word. Punctuation and operator spellings, matched against\n\
         /// the token text, which is the only place the matcher looks at the query again.\n\
         #[rustfmt::skip]\n\
         pub static SYMBOLS: [&str; {}] = [\n",
        table.symbols.len()
    ));
    for symbol in &table.symbols {
        out.push_str(&format!("    {symbol:?},\n"));
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
