//! Matching a path against a pattern, and finding the files a pattern names.
//!
//! `SELECT count(*) FROM read_parquet('data/*.parquet')` is how a directory of files is read in
//! every engine that reads one, so a path that reaches the filesystem has to be allowed to be a
//! pattern. The matching is done here rather than by a crate because it is a hundred lines and
//! because the rules have to be DuckDB's rather than a library's, and the rules were measured
//! against duckdb v1.4.1.
//!
//! A pattern is matched one path component at a time. `*` and `?` and a `[...]` class match inside a
//! component and never across a separator, so `data/*.parquet` does not reach into `data/sub`. `**`
//! is the one that crosses, matching any number of components including none, which is what makes
//! `**/*.parquet` find a file however deep it is.
//!
//! Two things here are decisions rather than copies.
//!
//! The answer is sorted. DuckDB hands back what the directory hands it, which on the ext4 box this
//! was measured on is the order the files were created in and on another filesystem is something
//! else. A query without an `ORDER BY` is not promised an order by SQL either way, so the choice is
//! between an order that is stable across machines and one that is not, and sorting is the one that
//! makes a test mean something.
//!
//! A pattern that matches nothing is not an error here. It is an error at the point where somebody
//! asked to read a file, because that is where DuckDB's sentence about it belongs and where the
//! pattern that was written is still around to be quoted. For the same reason nothing here fails:
//! a directory that cannot be listed contributes no matches, the way it does in a shell, so the only
//! answer is a list and a caller that wanted something in it says so itself.
//!
//! Brace expansion is not here, because DuckDB does not have it. `read_csv('{one,two}.csv')` is
//! `No files found that match the pattern` in the binary, measured rather than assumed, and a glob
//! here that understood braces would find files a compatible engine has to say it cannot find.

use std::path::{Component, Path, PathBuf};

use crate::Filesystem;

/// Whether this text has anything in it that makes it a pattern rather than a name.
///
/// A path without one of these three characters names one file, and the caller opens it rather than
/// listing a directory to find it. That matters for more than speed: a directory listing on a path
/// that is simply missing reports the parent, and the error somebody wants to read names the file
/// they asked for.
#[must_use]
pub fn is_pattern(text: &str) -> bool {
    text.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

/// Every file `pattern` names, sorted.
///
/// A pattern that matches nothing gives an empty list. A path with no pattern in it gives itself
/// when it exists, so a caller can hand everything through this and not care which it had.
#[must_use]
pub fn glob(filesystem: &dyn Filesystem, pattern: &str) -> Vec<PathBuf> {
    if !is_pattern(pattern) {
        let path = PathBuf::from(pattern);
        return if filesystem.exists(&path) { vec![path] } else { Vec::new() };
    }
    let path = Path::new(pattern);
    // The walk starts from whatever the pattern starts with, so an absolute pattern starts at the
    // root and a relative one starts where the process is. `PathBuf::new()` joined with a relative
    // name is that name, which is what keeps the answers relative when the question was.
    let mut here: Vec<PathBuf> = vec![PathBuf::new()];
    let parts: Vec<Component<'_>> = path.components().collect();
    for (at, part) in parts.iter().enumerate() {
        let last = at + 1 == parts.len();
        let Component::Normal(name) = part else {
            // A root, a drive, a `.` or a `..`. None of those can hold a pattern, so they are
            // pushed on and the walk carries on from there.
            for base in &mut here {
                base.push(part.as_os_str());
            }
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        let mut next = Vec::new();
        for base in &here {
            if name == "**" {
                // Every directory at or below this one, so the rest of the pattern is tried against
                // all of them. The current one is included, which is what makes `**` match nothing.
                descend(filesystem, base, &mut next);
                continue;
            }
            if !is_pattern(&name) {
                let joined = base.join(&name);
                if filesystem.exists(&joined) {
                    next.push(joined);
                }
                continue;
            }
            // A base that is not a directory contributes nothing, which is the case
            // `data/*/*.parquet` walks into as soon as `data` holds a file as well as a directory.
            for entry in listing(filesystem, base) {
                let Some(file) = entry.file_name() else { continue };
                if matches(&file.to_string_lossy(), &name) {
                    next.push(entry);
                }
            }
        }
        here = next;
        if here.is_empty() {
            return Vec::new();
        }
        if last {
            // A pattern names files, so a directory that matched the last component is not an
            // answer.
            here.retain(|path| !is_dir(filesystem, path));
        }
    }
    here.sort();
    here.dedup();
    here
}

/// `base` and every directory under it.
fn descend(filesystem: &dyn Filesystem, base: &Path, out: &mut Vec<PathBuf>) {
    out.push(base.to_path_buf());
    for entry in listing(filesystem, base) {
        if is_dir(filesystem, &entry) {
            descend(filesystem, &entry, out);
        }
    }
}

/// What is in a directory, and nothing for a path that is not one.
fn listing(filesystem: &dyn Filesystem, base: &Path) -> Vec<PathBuf> {
    if base.as_os_str().is_empty() {
        // A relative pattern lists the working directory and the answers have to stay relative, so
        // the `./` the listing comes back with is taken off again.
        let here = filesystem.read_dir(Path::new(".")).unwrap_or_default();
        return here
            .into_iter()
            .map(|path| path.strip_prefix(".").map_or(path.clone(), Path::to_path_buf))
            .collect();
    }
    filesystem.read_dir(base).unwrap_or_default()
}

/// Whether a path is a directory, which is whether it can be listed.
///
/// There is no `is_dir` on the filesystem trait and this is the only caller that wants one. An
/// object store has no directories at all, so a question phrased as "can this be listed" is one
/// every backend can answer, where "is this a directory" is one some of them would have to make up.
fn is_dir(filesystem: &dyn Filesystem, path: &Path) -> bool {
    filesystem.read_dir(path).is_ok()
}

/// Whether one name matches one pattern, with `*` and `?` stopping at a separator.
///
/// The walk above hands this one path component at a time, so the separator rule never comes up
/// there. It is here anyway, because this is public and the rule is what everybody means by a glob:
/// `*.parquet` names a file in this directory and not a file in a directory under it.
///
/// Written as a walk with one backtracking point rather than as a recursion, because the only thing
/// that needs to be undone is the last `*`, and a name of a thousand characters against a pattern of
/// a thousand stars should not be a thousand stack frames. That case is not hypothetical the way it
/// sounds: the exponential version of this is a well known way to hang a process on an input
/// somebody else wrote.
#[must_use]
pub fn matches(name: &str, pattern: &str) -> bool {
    let text: Vec<char> = name.chars().collect();
    let glob: Vec<char> = pattern.chars().collect();
    let (mut at, mut pat) = (0, 0);
    // Where to resume if the rest of the pattern turns out not to fit: the last star and the
    // character after the point it had matched up to.
    let (mut star, mut resume) = (None, 0);
    while at < text.len() {
        match step(&glob, pat, text[at]) {
            Step::Star => {
                star = Some(pat);
                resume = at;
                pat += 1;
            }
            Step::Eat(next) => {
                at += 1;
                pat = next;
            }
            // The star eats one more character and the rest of the pattern is tried again, unless
            // the character it would have to eat is a separator, which no star crosses.
            Step::No => match star {
                Some(back) if plain(text[resume]) => {
                    pat = back + 1;
                    resume += 1;
                    at = resume;
                }
                _ => return false,
            },
        }
    }
    glob[pat..].iter().all(|&left| left == '*')
}

/// What one pattern character does against one name character.
enum Step {
    /// A star, which matches nothing here and is remembered in case it has to match more later.
    Star,
    /// One character matched, and the pattern carries on from here.
    Eat(usize),
    /// It does not match.
    No,
}

/// Which of the three this position is.
fn step(glob: &[char], pat: usize, have: char) -> Step {
    match glob.get(pat) {
        Some('*') => Step::Star,
        Some('?') if plain(have) => Step::Eat(pat + 1),
        Some('[') => match class(glob, pat, have) {
            Some((true, end)) if plain(have) => Step::Eat(end),
            Some(_) => Step::No,
            // Never closed, so the bracket was an ordinary character all along.
            None if have == '[' => Step::Eat(pat + 1),
            None => Step::No,
        },
        Some(&literal) if literal == have => Step::Eat(pat + 1),
        _ => Step::No,
    }
}

/// Whether a character is one a `*` or a `?` is allowed to stand for.
///
/// Both separators, because a Windows path is written with one and a pattern somebody typed on
/// Windows is read on the machine it was typed on.
fn plain(have: char) -> bool {
    have != '/' && have != '\\'
}

/// Whether a `[...]` class starting at `open` accepts `have`, and where the class ends.
///
/// `[!abc]` and `[^abc]` are the negated form, `[a-z]` is a range, and a `]` straight after the
/// opening bracket is the bracket itself rather than the end of the class. A class that is never
/// closed is not a class at all, which is the `None`, and the bracket is then an ordinary character.
/// That is the difference between `[a.parquet` naming a file that is really called that and naming
/// nothing.
fn class(glob: &[char], open: usize, have: char) -> Option<(bool, usize)> {
    let mut at = open + 1;
    let negated = matches!(glob.get(at), Some('!' | '^'));
    if negated {
        at += 1;
    }
    let mut hit = false;
    let mut first = true;
    while at < glob.len() {
        if glob[at] == ']' && !first {
            return Some((hit != negated, at + 1));
        }
        first = false;
        let from = glob[at];
        if glob.get(at + 1) == Some(&'-') && glob.get(at + 2).is_some_and(|&end| end != ']') {
            let to = glob[at + 2];
            hit |= (from..=to).contains(&have);
            at += 3;
            continue;
        }
        hit |= from == have;
        at += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{OpenMode, SimFilesystem};

    #[test]
    fn a_name_with_none_of_the_three_characters_in_it_is_not_a_pattern() {
        assert!(!is_pattern("hits.parquet"));
        assert!(is_pattern("*.parquet"));
        assert!(is_pattern("hit?.parquet"));
        assert!(is_pattern("hits[0-9].parquet"));
    }

    #[test]
    fn a_star_matches_any_run_and_a_question_mark_matches_one() {
        assert!(matches("hits.parquet", "*.parquet"));
        assert!(matches("hits.parquet", "hits*"));
        assert!(matches("hits.parquet", "*"));
        assert!(matches("one.parquet", "?ne.parquet"));
        assert!(!matches("one.parquet", "?e.parquet"));
        assert!(!matches("hits.csv", "*.parquet"));
    }

    /// The case a recursive matcher gets wrong by being exponential rather than by being incorrect.
    #[test]
    fn a_run_of_stars_against_a_long_name_still_answers() {
        let name = "a".repeat(200);
        assert!(matches(&name, &"*a*a*a*a*b".replace('b', "a")));
        assert!(!matches(&name, "*a*a*a*a*b"));
    }

    #[test]
    fn a_star_and_a_question_mark_both_stop_at_a_separator() {
        assert!(!matches("sub/deep.parquet", "*.parquet"));
        assert!(!matches("sub/deep.parquet", "sub?deep.parquet"));
        assert!(!matches("sub\\deep.parquet", "*.parquet"));
        // A separator written in the pattern is an ordinary character and still matches.
        assert!(matches("sub/deep.parquet", "sub/*.parquet"));
    }

    #[test]
    fn a_class_takes_a_list_a_range_and_a_negation() {
        assert!(matches("a.parquet", "[abc].parquet"));
        assert!(!matches("d.parquet", "[abc].parquet"));
        assert!(matches("part7.parquet", "part[0-9].parquet"));
        assert!(!matches("parta.parquet", "part[0-9].parquet"));
        assert!(matches("d.parquet", "[!abc].parquet"));
        assert!(!matches("a.parquet", "[^abc].parquet"));
        // A bracket that is never closed is a bracket.
        assert!(matches("[a.parquet", "[a.parquet"));
    }

    /// A filesystem holding these files, and nothing else.
    fn holding(paths: &[&str]) -> SimFilesystem {
        let filesystem = SimFilesystem::new();
        for path in paths {
            let path = Path::new(path);
            if let Some(parent) = path.parent() {
                filesystem.create_dir_all(parent).expect("makes the directory");
            }
            filesystem.open(path, OpenMode::CreateNew).expect("makes the file");
        }
        filesystem
    }

    fn found(filesystem: &SimFilesystem, pattern: &str) -> Vec<String> {
        glob(filesystem, pattern).iter().map(|path| path.display().to_string()).collect()
    }

    #[test]
    fn a_pattern_finds_the_files_in_one_directory_sorted() {
        let filesystem = holding(&["/data/c.parquet", "/data/a.parquet", "/data/b.csv"]);
        assert_eq!(found(&filesystem, "/data/*.parquet"), ["/data/a.parquet", "/data/c.parquet"]);
        assert_eq!(
            found(&filesystem, "/data/*"),
            ["/data/a.parquet", "/data/b.csv", "/data/c.parquet"]
        );
    }

    #[test]
    fn a_pattern_in_one_component_does_not_reach_into_another() {
        let filesystem = holding(&["/data/a.parquet", "/data/sub/b.parquet"]);
        assert_eq!(found(&filesystem, "/data/*.parquet"), ["/data/a.parquet"]);
        assert_eq!(found(&filesystem, "/data/*/*.parquet"), ["/data/sub/b.parquet"]);
    }

    #[test]
    fn two_stars_cross_directories_and_match_none_of_them_as_well() {
        let filesystem = holding(&["/data/a.parquet", "/data/sub/deep/b.parquet"]);
        assert_eq!(
            found(&filesystem, "/data/**/*.parquet"),
            ["/data/a.parquet", "/data/sub/deep/b.parquet"]
        );
    }

    #[test]
    fn a_directory_that_matches_the_last_component_is_not_a_file() {
        let filesystem = holding(&["/data/sub/b.parquet"]);
        assert!(found(&filesystem, "/data/*").is_empty(), "sub is a directory");
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_an_empty_list_and_not_an_error() {
        let filesystem = holding(&["/data/a.parquet"]);
        assert!(found(&filesystem, "/data/*.csv").is_empty());
        assert!(found(&filesystem, "/nowhere/*.parquet").is_empty());
    }

    #[test]
    fn a_path_that_is_not_a_pattern_is_itself_when_it_is_there() {
        let filesystem = holding(&["/data/a.parquet"]);
        assert_eq!(found(&filesystem, "/data/a.parquet"), ["/data/a.parquet"]);
        assert!(found(&filesystem, "/data/b.parquet").is_empty());
    }
}
