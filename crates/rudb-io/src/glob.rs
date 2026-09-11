//! Path patterns, and the filesystem walk that turns one into a list of files.
//!
//! `SELECT * FROM 'hits/*.parquet'` is how a directory of files is read in DuckDB, and the pattern
//! is expanded before anything is opened, so a pattern that matches nothing is an error at bind time
//! rather than an empty result at run time. That is the behaviour this reproduces, along with the
//! four pieces of syntax duckdb v1.4.1 actually takes, which were measured rather than assumed.
//!
//! `*` matches any run of characters inside one path segment and does not cross a separator, so
//! `a/*/c` reaches one level down and no further. `?` matches one character. `[abc]` matches one of
//! the characters listed, `[a-z]` takes a range and `[!abc]` or `[^abc]` takes the complement. `**`
//! is a segment of its own and matches any number of directories including none, so `a/**/*.parquet`
//! finds the parquet files directly under `a` as well as the ones further down.
//!
//! Brace expansion is not here because duckdb v1.4.1 does not have it either. `{p1,p2}.parquet` is
//! an ordinary file name there and matches a file with braces in it, which was measured.
//!
//! The order is the sort order of the whole path, as a string of bytes, which is also measured. It
//! matters because a query over a set of files concatenates them, so the order the walk returns is
//! the order the rows come out in, and an order that came from the directory's own layout would make
//! `LIMIT 3` answer differently on two machines holding the same files.

use std::path::{Path, PathBuf};

use rudb_common::Result;

use crate::Filesystem;

/// The characters that make a pattern a pattern rather than a name.
const MAGIC: [char; 3] = ['*', '?', '['];

/// Whether this is a pattern at all.
///
/// A path with none of these in it is a file name, and a file name is looked for rather than walked
/// to, which saves reading every directory on the way down for the ordinary case.
#[must_use]
pub fn has_magic(pattern: &str) -> bool {
    pattern.contains(MAGIC)
}

/// Every file under `filesystem` whose path matches `pattern`, sorted.
///
/// A pattern with no magic in it is itself, if it is there, and nothing otherwise. The caller is the
/// one that turns nothing into an error, because what it says depends on what asked.
///
/// # Errors
///
/// A directory that cannot be read, except that a directory that is not there is not an error: a
/// pattern is allowed to name a place nothing is.
pub fn expand(filesystem: &dyn Filesystem, pattern: &str) -> Result<Vec<String>> {
    if !has_magic(pattern) {
        let found = if filesystem.exists(Path::new(pattern)) {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        };
        return Ok(found);
    }
    let (root, rest) = split_root(pattern);
    let mut here = vec![root];
    for segment in rest {
        here = step(filesystem, &here, segment)?;
        if here.is_empty() {
            break;
        }
    }
    let mut found: Vec<String> = here
        .into_iter()
        .filter(|path| filesystem.exists(path) && !filesystem.is_dir(path))
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    found.sort();
    found.dedup();
    Ok(found)
}

/// The part of the pattern before the first segment that has magic in it, and the segments after it.
///
/// The leading run of ordinary names is kept whole rather than walked, so an absolute pattern starts
/// from `/` and a relative one starts from the empty path, and neither needs the walk to know which
/// it is looking at.
fn split_root(pattern: &str) -> (PathBuf, Vec<&str>) {
    let mut segments = pattern.split('/').peekable();
    let mut root = PathBuf::new();
    if pattern.starts_with('/') {
        root.push("/");
        let _ = segments.next();
    }
    while let Some(segment) = segments.peek() {
        if has_magic(segment) || *segment == "**" {
            break;
        }
        root.push(segments.next().unwrap_or_default());
    }
    (root, segments.collect())
}

/// One segment of the pattern applied to every place the walk has reached.
fn step(filesystem: &dyn Filesystem, here: &[PathBuf], segment: &str) -> Result<Vec<PathBuf>> {
    let mut next = Vec::new();
    for base in here {
        if segment == "**" {
            // Zero directories as well as many, so `a/**/*.parquet` finds what is directly under
            // `a`. That is DuckDB's reading of it and it is the one people expect.
            next.push(base.clone());
            descend(filesystem, base, &mut next)?;
            continue;
        }
        if !has_magic(segment) {
            next.push(base.join(segment));
            continue;
        }
        for entry in read_dir(filesystem, base)? {
            let name = entry.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if matches(segment, &name) {
                next.push(entry);
            }
        }
    }
    Ok(next)
}

/// Every directory at or below `base`, not including `base` itself.
fn descend(filesystem: &dyn Filesystem, base: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in read_dir(filesystem, base)? {
        if filesystem.is_dir(&entry) {
            descend(filesystem, &entry, out)?;
            out.push(entry);
        }
    }
    Ok(())
}

/// The entries of a directory, and nothing for a directory that is not one or is not there.
fn read_dir(filesystem: &dyn Filesystem, path: &Path) -> Result<Vec<PathBuf>> {
    if !filesystem.is_dir(path) {
        return Ok(Vec::new());
    }
    filesystem.read_dir(path)
}

/// Whether one name matches one segment of a pattern.
///
/// A hand written matcher rather than a translation into a regular expression, because the pattern
/// language is four pieces of syntax and the translation would have to escape everything that is not
/// one of them, which is the part that gets a case wrong.
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    at(&pattern, 0, &name, 0)
}

/// Whether the pattern from `p` matches the name from `n`.
///
/// Backtracking, which is exponential on a pattern of many stars against a name that nearly matches.
/// That is a pattern somebody would have to write on purpose and the names here are the length of a
/// file name, so the linear time version with its two saved positions is a complication with nothing
/// asking for it yet.
fn at(pattern: &[char], p: usize, name: &[char], n: usize) -> bool {
    let Some(&current) = pattern.get(p) else { return n >= name.len() };
    match current {
        '*' => (n..=name.len()).any(|skip| at(pattern, p + 1, name, skip)),
        '?' => n < name.len() && at(pattern, p + 1, name, n + 1),
        '[' => match class(pattern, p, name.get(n).copied()) {
            Some(after) => at(pattern, after, name, n + 1),
            None => false,
        },
        _ => name.get(n) == Some(&current) && at(pattern, p + 1, name, n + 1),
    }
}

/// Matches one character against the class starting at `p`, and answers where the class ended.
///
/// `None` means the character did not match, which is also the answer for a class that was never
/// closed, because a lone `[` is not a class and a name with a `[` in it is a name.
fn class(pattern: &[char], p: usize, against: Option<char>) -> Option<usize> {
    let mut at = p + 1;
    let negated = matches!(pattern.get(at), Some('!' | '^'));
    if negated {
        at += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let &current = pattern.get(at)?;
        // A `]` in the first position is a literal `]`, which is how the character is written at all.
        if current == ']' && !first {
            at += 1;
            break;
        }
        first = false;
        let ranged = matches!(pattern.get(at + 1), Some('-'))
            && !matches!(pattern.get(at + 2), Some(']') | None);
        if ranged {
            let high = *pattern.get(at + 2)?;
            if against.is_some_and(|c| c >= current && c <= high) {
                hit = true;
            }
            at += 3;
            continue;
        }
        if against == Some(current) {
            hit = true;
        }
        at += 1;
    }
    // A negated class still has to have a character to not match, so an empty name matches nothing.
    if hit != negated && against.is_some() { Some(at) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpenMode, SimFilesystem};

    #[test]
    fn a_star_matches_a_run_of_anything_including_nothing() {
        assert!(matches("*.parquet", "hits.parquet"));
        assert!(matches("*.parquet", ".parquet"));
        assert!(matches("hits*", "hits"));
        assert!(!matches("*.parquet", "hits.csv"));
    }

    #[test]
    fn a_question_mark_matches_one_character_and_insists_on_one() {
        assert!(matches("p?.parquet", "p1.parquet"));
        assert!(!matches("p?.parquet", "p.parquet"));
        assert!(!matches("p?.parquet", "p12.parquet"));
    }

    #[test]
    fn a_class_matches_one_of_what_it_lists() {
        assert!(matches("p[12].parquet", "p1.parquet"));
        assert!(matches("p[12].parquet", "p2.parquet"));
        assert!(!matches("p[12].parquet", "p3.parquet"));
        assert!(matches("[a-z]ame", "name"));
        assert!(!matches("[a-z]ame", "Name"));
    }

    #[test]
    fn a_negated_class_matches_what_it_does_not_list() {
        assert!(matches("p[!12].parquet", "p3.parquet"));
        assert!(!matches("p[!12].parquet", "p1.parquet"));
        assert!(matches("p[^12].parquet", "p3.parquet"));
    }

    #[test]
    fn a_class_that_is_never_closed_is_not_a_class_and_matches_nothing() {
        assert!(!matches("p[12.parquet", "p1.parquet"));
    }

    #[test]
    fn several_stars_still_settle_on_an_answer() {
        assert!(matches("*a*b*", "xxaybzz"));
        assert!(!matches("*a*b*c", "xxaybzz"));
    }

    #[test]
    fn a_name_with_no_magic_in_it_is_not_a_pattern() {
        assert!(!has_magic("/data/hits.parquet"));
        assert!(has_magic("/data/*.parquet"));
        assert!(has_magic("/data/p?.parquet"));
        assert!(has_magic("/data/p[12].parquet"));
        // Braces are not magic, which is duckdb v1.4.1's answer and was measured.
        assert!(!has_magic("/data/{p1,p2}.parquet"));
    }

    /// A filesystem holding these paths, each with a byte in it so that it is a file.
    fn holding(paths: &[&str]) -> SimFilesystem {
        let filesystem = SimFilesystem::new();
        for path in paths {
            let at = Path::new(path);
            if let Some(parent) = at.parent() {
                filesystem.create_dir_all(parent).expect("makes the directory");
            }
            filesystem.open(at, OpenMode::Create).expect("makes the file");
        }
        filesystem
    }

    #[test]
    fn a_pattern_with_no_magic_is_the_file_it_names_or_nothing() {
        let filesystem = holding(&["/data/hits.parquet"]);
        assert_eq!(
            expand(&filesystem, "/data/hits.parquet").expect("walks"),
            ["/data/hits.parquet"]
        );
        assert!(expand(&filesystem, "/data/other.parquet").expect("walks").is_empty());
    }

    #[test]
    fn a_star_picks_the_files_of_one_directory_and_sorts_them() {
        let filesystem = holding(&[
            "/data/p2.parquet",
            "/data/p1.parquet",
            "/data/notes.csv",
            "/data/deep/p3.parquet",
        ]);
        assert_eq!(
            expand(&filesystem, "/data/*.parquet").expect("walks"),
            ["/data/p1.parquet", "/data/p2.parquet"]
        );
    }

    #[test]
    fn a_star_does_not_cross_a_separator_and_a_middle_one_reaches_one_level() {
        let filesystem = holding(&[
            "/data/top.parquet",
            "/data/one/a.parquet",
            "/data/two/b.parquet",
            "/data/two/deep/c.parquet",
        ]);
        assert_eq!(
            expand(&filesystem, "/data/*/*.parquet").expect("walks"),
            ["/data/one/a.parquet", "/data/two/b.parquet"]
        );
    }

    #[test]
    fn a_double_star_reaches_any_depth_including_none() {
        let filesystem =
            holding(&["/data/top.parquet", "/data/one/a.parquet", "/data/two/deep/c.parquet"]);
        assert_eq!(
            expand(&filesystem, "/data/**/*.parquet").expect("walks"),
            ["/data/one/a.parquet", "/data/top.parquet", "/data/two/deep/c.parquet"]
        );
    }

    #[test]
    fn a_directory_is_not_a_file_and_does_not_come_back_as_one() {
        let filesystem = holding(&["/data/one/a.parquet"]);
        assert!(expand(&filesystem, "/data/*").expect("walks").is_empty());
    }

    #[test]
    fn a_pattern_over_a_place_that_is_not_there_is_empty_rather_than_an_error() {
        let filesystem = holding(&["/data/a.parquet"]);
        assert!(expand(&filesystem, "/nowhere/*.parquet").expect("walks").is_empty());
    }

    #[test]
    fn a_relative_pattern_stays_relative() {
        let filesystem = holding(&["data/a.parquet", "data/b.parquet"]);
        assert_eq!(
            expand(&filesystem, "data/*.parquet").expect("walks"),
            ["data/a.parquet", "data/b.parquet"]
        );
    }
}
