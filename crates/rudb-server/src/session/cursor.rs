//! The cursor statements of PostgreSQL: `DECLARE`, `FETCH`, `MOVE` and `CLOSE`.
//!
//! A cursor is a portal with a name, in the same namespace as the portals of `Bind`, so the
//! server runs these statements itself and does not give them to the engine. This module reads
//! the statements with the grammar of `gram.y` and moves the place of a cursor as
//! `DoPortalRunFetch` and `PortalRunSelect` move it. A statement that this reader cannot read
//! goes to the engine, which gives the syntax error.

use super::setting::{Token, loose, spanned};

/// A statement on a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cursor {
    Declare(Declare),
    /// `FETCH`, or `MOVE` with `moves` true.
    Fetch {
        name: String,
        direction: Direction,
        moves: bool,
    },
    /// `CLOSE name`, or `CLOSE ALL` with `None`.
    Close(Option<String>),
}

/// `DECLARE name CURSOR FOR query`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Declare {
    pub(super) name: String,
    /// `BINARY`: the rows of `FETCH` on the simple flow go out in the binary format.
    pub(super) binary: bool,
    /// `SCROLL` with `Some(true)`, `NO SCROLL` with `Some(false)`.
    pub(super) scroll: Option<bool>,
    /// `WITH HOLD`: the cursor stays after the transaction commits.
    pub(super) hold: bool,
    /// The byte offset of the query in the statement.
    pub(super) query: usize,
}

/// The move of a `FETCH` or a `MOVE`, with the forms of the grammar folded as `gram.y` folds
/// them: `NEXT` is `FORWARD 1`, `PRIOR` is `BACKWARD 1`, `FIRST` is `ABSOLUTE 1`, `LAST` is
/// `ABSOLUTE -1` and `ALL` is `FORWARD ALL`. A count of [`ALL`] is all the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    Forward(u64),
    Backward(u64),
    Absolute(i64),
    Relative(i64),
}

/// The count of `ALL`.
pub(super) const ALL: u64 = u64::MAX;

/// Reads a statement on a cursor, or gives `None` for another statement.
pub(super) fn parse(sql: &str) -> Option<Cursor> {
    let head = sql.trim_start().as_bytes();
    let starts = |word: &str| {
        head.get(..word.len()).is_some_and(|h| h.eq_ignore_ascii_case(word.as_bytes()))
    };
    if !["declare", "fetch", "move", "close"].iter().any(|w| starts(w)) {
        return None;
    }
    if starts("declare") {
        // Only the head up to `FOR` is read here, since the query has the full grammar.
        let end = head_end(sql)?;
        let (tokens, _) = spanned(&sql[..end])?;
        let mut p = Reader { tokens: &tokens, at: 1 };
        let declare = p.declare()?;
        let query = sql[end..].trim_start();
        if query.trim_end_matches([';', ' ', '\t', '\r', '\n']).is_empty() {
            return None;
        }
        return Some(Cursor::Declare(Declare { query: sql.len() - query.len(), ..declare }));
    }
    let (mut tokens, _) = spanned(sql)?;
    while tokens.last() == Some(&Token::Punct(';')) {
        tokens.pop();
    }
    let mut p = Reader { tokens: &tokens, at: 1 };
    let cursor = match tokens.first()? {
        first if first.is("fetch") => p.fetch(false)?,
        first if first.is("move") => p.fetch(true)?,
        first if first.is("close") => {
            if p.eat("all") {
                Cursor::Close(None)
            } else {
                Cursor::Close(Some(p.name()?))
            }
        }
        _ => return None,
    };
    (p.at == tokens.len()).then_some(cursor)
}

/// The end of the first word `FOR` of a `DECLARE`, which ends its head. A quoted name can hold
/// the word, so the quotes are skipped.
fn head_end(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'"' {
            at += 1;
            while at < bytes.len() && bytes[at] != b'"' {
                at += 1;
            }
            at += 1;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            if sql[start..at].eq_ignore_ascii_case("for") {
                return Some(at);
            }
        } else {
            at += 1;
        }
    }
    None
}

struct Reader<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn eat(&mut self, word: &str) -> bool {
        let found = self.peek().is_some_and(|token| token.is(word));
        self.at += usize::from(found);
        found
    }

    fn name(&mut self) -> Option<String> {
        let Token::Word { text, .. } = self.peek()? else {
            return None;
        };
        let name = text.clone();
        self.at += 1;
        Some(name)
    }

    /// A count of `SignedIconst`, which is an `integer`.
    fn count(&mut self) -> Option<i64> {
        let negative = match self.peek()? {
            Token::Punct('-') => true,
            Token::Punct('+') => false,
            Token::Number { integer: true, .. } => {
                return self.unsigned();
            }
            _ => return None,
        };
        self.at += 1;
        let count = self.unsigned()?;
        Some(if negative { -count } else { count })
    }

    fn unsigned(&mut self) -> Option<i64> {
        let Some(Token::Number { text, integer: true }) = self.peek() else {
            return None;
        };
        let count = text.parse::<i32>().ok()?;
        self.at += 1;
        Some(i64::from(count))
    }

    fn is_count(&self) -> bool {
        matches!(self.peek(), Some(Token::Number { integer: true, .. } | Token::Punct('-' | '+')))
    }

    fn declare(&mut self) -> Option<Declare> {
        let name = self.name()?;
        let mut declare = Declare { name, binary: false, scroll: None, hold: false, query: 0 };
        loop {
            if self.eat("binary") {
                declare.binary = true;
            } else if self.eat("scroll") {
                declare.scroll = Some(true);
            } else if self.eat("no") {
                if !self.eat("scroll") {
                    return None;
                }
                declare.scroll = Some(false);
            } else if !self.eat("insensitive") && !self.eat("asensitive") {
                break;
            }
        }
        if !self.eat("cursor") {
            return None;
        }
        if self.eat("with") {
            declare.hold = self.eat("hold").then_some(true)?;
        } else if self.eat("without") {
            self.eat("hold").then_some(())?;
        }
        (self.eat("for") && self.at == self.tokens.len()).then_some(declare)
    }

    fn fetch(&mut self, moves: bool) -> Option<Cursor> {
        let signed = |count: i64| match count {
            ..0 => Direction::Backward(count.unsigned_abs()),
            _ => Direction::Forward(count.unsigned_abs()),
        };
        let direction = if self.eat("next") {
            Direction::Forward(1)
        } else if self.eat("prior") {
            Direction::Backward(1)
        } else if self.eat("first") {
            Direction::Absolute(1)
        } else if self.eat("last") {
            Direction::Absolute(-1)
        } else if self.eat("absolute") {
            Direction::Absolute(self.count()?)
        } else if self.eat("relative") {
            Direction::Relative(self.count()?)
        } else if self.eat("all") {
            Direction::Forward(ALL)
        } else if self.eat("forward") {
            if self.eat("all") {
                Direction::Forward(ALL)
            } else if self.is_count() {
                signed(self.count()?)
            } else {
                Direction::Forward(1)
            }
        } else if self.eat("backward") {
            if self.eat("all") {
                Direction::Backward(ALL)
            } else if self.is_count() {
                match signed(self.count()?) {
                    Direction::Forward(count) => Direction::Backward(count),
                    Direction::Backward(count) => Direction::Forward(count),
                    other => other,
                }
            } else {
                Direction::Backward(1)
            }
        } else if self.is_count() {
            signed(self.count()?)
        } else {
            Direction::Forward(1)
        };
        let _ = self.eat("from") || self.eat("in");
        let name = self.name()?;
        Some(Cursor::Fetch { name, direction, moves })
    }
}

/// The first word of the query of a `DECLARE` when the grammar does not take it there, as in
/// `DECLARE c CURSOR FOR INSERT ...`, which is a syntax error at that word.
pub(super) fn not_a_query(query: &str) -> Option<&str> {
    let query = query.trim_start();
    if query.starts_with('(') {
        return None;
    }
    let end = query.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(query.len());
    let word = &query[..end];
    let known = ["select", "values", "table", "with"];
    (!known.iter().any(|known| word.eq_ignore_ascii_case(known))).then_some(word)
}

/// The aggregates that a query most often calls. A plan with an aggregate cannot run backward.
const AGGREGATES: [&str; 12] = [
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "array_agg",
    "string_agg",
    "json_agg",
    "jsonb_agg",
    "bool_and",
    "bool_or",
    "every",
];

/// True when a cursor with no `SCROLL` and no `NO SCROLL` can move back. PostgreSQL asks the plan
/// with `ExecSupportsBackwardScan`: a scan, a sort and a limit can run backward, but a join, an
/// aggregate, a set operation, a window, `DISTINCT` and a query without `FROM` cannot. This
/// reads the same from the words of the query.
pub(super) fn scrolls(query: &str) -> bool {
    let Some(tokens) = loose(query) else {
        return false;
    };
    let stops = ["join", "group", "having", "distinct", "union", "intersect", "except", "over"];
    let ends = ["where", "order", "limit", "offset", "fetch", "for", "window"];
    let mut depth = 0usize;
    let mut source =
        matches!(tokens.first(), Some(first) if first.is("values") || first.is("table"));
    let mut in_from = false;
    for (at, token) in tokens.iter().enumerate() {
        let call = tokens.get(at + 1) == Some(&Token::Punct('('));
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Punct(',') if depth == 0 && in_from => return false,
            _ if stops.iter().any(|stop| token.is(stop)) => return false,
            _ if call && AGGREGATES.iter().any(|name| token.is(name)) => return false,
            _ if depth == 0 && token.is("from") => (source, in_from) = (true, true),
            _ if depth == 0 && ends.iter().any(|end| token.is(end)) => in_from = false,
            _ => {}
        }
    }
    source
}

/// The error of a move back on a cursor that is not `SCROLL`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct NoScroll;

/// The rows of one move, numbered from 1, down from `first` when `forward` is false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Run {
    pub(super) first: u64,
    pub(super) count: u64,
    pub(super) forward: bool,
}

impl Run {
    const NONE: Run = Run { first: 0, count: 0, forward: true };
}

/// The place of a cursor in its rows: 0 before the first row, `n` on row `n`, and one past the
/// last row after the end. This is `portalPos` of PostgreSQL, with `atEnd` as the place after the
/// last row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Place {
    pub(super) at: u64,
}

impl Place {
    /// `PortalRunSelect` forward: up to `count` rows after the place.
    pub(super) fn forward(&mut self, len: u64, count: u64) -> Run {
        if count == 0 || self.at > len {
            return Run::NONE;
        }
        let first = self.at + 1;
        let last = self.at.saturating_add(count).min(len);
        let taken = (last + 1).saturating_sub(first);
        self.at = if taken < count { len + 1 } else { last };
        Run { first, count: taken, forward: true }
    }

    /// `PortalRunSelect` backward: up to `count` rows before the place, nearest first.
    pub(super) fn backward(&mut self, scroll: bool, count: u64) -> Result<Run, NoScroll> {
        if !scroll {
            return Err(NoScroll);
        }
        if count == 0 || self.at == 0 {
            return Ok(Run::NONE);
        }
        let first = self.at - 1;
        let taken = count.min(first);
        self.at = if taken < count { 0 } else { self.at - count };
        Ok(Run { first, count: taken, forward: false })
    }

    /// `DoPortalRewind`: back to the place before the first row.
    fn rewind(&mut self, scroll: bool) -> Result<(), NoScroll> {
        if self.at == 0 {
            return Ok(());
        }
        if !scroll {
            return Err(NoScroll);
        }
        self.at = 0;
        Ok(())
    }

    /// `DoPortalRunFetch`: makes the move and gives the rows that a `FETCH` sends and the count
    /// that a `MOVE` gives.
    pub(super) fn fetch(
        &mut self,
        len: u64,
        scroll: bool,
        direction: Direction,
        moves: bool,
    ) -> Result<(Run, u64), NoScroll> {
        let done = |run: Run| (run, run.count);
        let (forward, count) = match direction {
            Direction::Absolute(count) if count > 0 => {
                let count = count.unsigned_abs();
                let at = if self.at > len { len } else { self.at };
                if count - 1 <= at / 2 {
                    self.rewind(scroll)?;
                    if count > 1 {
                        self.forward(len, count - 1);
                    }
                } else if count <= self.at {
                    self.backward(scroll, self.at - count + 1)?;
                } else if count > self.at + 1 {
                    self.forward(len, count - self.at - 1);
                }
                return Ok(done(self.forward(len, 1)));
            }
            Direction::Absolute(count) if count < 0 => {
                self.forward(len, ALL);
                if count < -1 {
                    self.backward(scroll, count.unsigned_abs() - 1)?;
                }
                return Ok(done(self.backward(scroll, 1)?));
            }
            Direction::Absolute(_) => {
                self.rewind(scroll)?;
                return Ok(done(Run::NONE));
            }
            Direction::Relative(count) if count > 0 => {
                if count > 1 {
                    self.forward(len, count.unsigned_abs() - 1);
                }
                return Ok(done(self.forward(len, 1)));
            }
            Direction::Relative(count) if count < 0 => {
                if count < -1 {
                    self.backward(scroll, count.unsigned_abs() - 1)?;
                }
                return Ok(done(self.backward(scroll, 1)?));
            }
            Direction::Relative(_) => (true, 0),
            Direction::Forward(count) => (true, count),
            Direction::Backward(count) => (false, count),
        };
        let (mut forward, mut count) = (forward, count);
        // A count of 0 sends the row the cursor is on again, if it is on one.
        if count == 0 {
            let on_row = self.at >= 1 && self.at <= len;
            if moves {
                return Ok((Run::NONE, u64::from(on_row)));
            }
            if on_row {
                self.backward(scroll, 1)?;
                (forward, count) = (true, 1);
            }
        }
        if !forward && count == ALL && moves {
            let moved = if self.at > len { len } else { self.at.saturating_sub(1) };
            self.rewind(scroll)?;
            return Ok((Run::NONE, moved));
        }
        let run = if forward { self.forward(len, count) } else { self.backward(scroll, count)? };
        Ok(done(run))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declare(sql: &str) -> Declare {
        match parse(sql) {
            Some(Cursor::Declare(declare)) => declare,
            other => panic!("{other:?}"),
        }
    }

    fn direction(sql: &str) -> Direction {
        match parse(sql) {
            Some(Cursor::Fetch { direction, .. }) => direction,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_statements_read_as_in_the_grammar() {
        let sql = "DECLARE c BINARY NO SCROLL CURSOR WITH HOLD FOR SELECT 1";
        let found = declare(sql);
        assert_eq!((found.binary, found.scroll, found.hold), (true, Some(false), true));
        assert_eq!(&sql[found.query..], "SELECT 1");
        assert_eq!(declare("declare \"C\" cursor for values (1);").name, "C");
        let sql = "declare \"for\" cursor for select a::int * 2 / 1 from t";
        assert_eq!(&sql[declare(sql).query..], "select a::int * 2 / 1 from t");
        assert_eq!(parse("declare c cursor for ;"), None);
        assert_eq!(parse("declare c cursor"), None);
        assert_eq!(parse("declare c cursor with for select 1"), None);
        assert_eq!(direction("fetch c"), Direction::Forward(1));
        assert_eq!(direction("fetch next from c"), Direction::Forward(1));
        assert_eq!(direction("fetch prior in c"), Direction::Backward(1));
        assert_eq!(direction("fetch last c"), Direction::Absolute(-1));
        assert_eq!(direction("fetch absolute -2 c"), Direction::Absolute(-2));
        assert_eq!(direction("fetch -3 c"), Direction::Backward(3));
        assert_eq!(direction("fetch all c"), Direction::Forward(ALL));
        assert_eq!(direction("fetch forward -1 c"), Direction::Backward(1));
        assert_eq!(direction("fetch backward -1 c"), Direction::Forward(1));
        assert_eq!(direction("move backward all in c"), Direction::Backward(ALL));
        assert_eq!(
            parse("move c"),
            Some(Cursor::Fetch { name: "c".into(), direction: Direction::Forward(1), moves: true })
        );
        assert_eq!(parse("close all"), Some(Cursor::Close(None)));
        assert_eq!(parse("close c;"), Some(Cursor::Close(Some("c".into()))));
        assert_eq!(parse("fetch 1 c d"), None);
        assert_eq!(parse("closet"), None);
    }

    /// The rows that a `FETCH` sends on rows 1 to 5, as a list.
    fn rows(place: &mut Place, direction: Direction) -> Vec<u64> {
        let (run, _) = place.fetch(5, true, direction, false).unwrap();
        (0..run.count).map(|i| if run.forward { run.first + i } else { run.first - i }).collect()
    }

    #[test]
    fn a_cursor_moves_as_in_postgresql() {
        let mut place = Place::default();
        assert_eq!(rows(&mut place, Direction::Forward(2)), [1, 2]);
        assert_eq!(rows(&mut place, Direction::Forward(0)), [2]);
        assert_eq!(rows(&mut place, Direction::Relative(2)), [4]);
        assert_eq!(rows(&mut place, Direction::Backward(ALL)), [3, 2, 1]);
        assert_eq!(rows(&mut place, Direction::Backward(1)), Vec::<u64>::new());
        assert_eq!(rows(&mut place, Direction::Absolute(-1)), [5]);
        assert_eq!(rows(&mut place, Direction::Forward(1)), Vec::<u64>::new());
        assert_eq!(place.at, 6);
        assert_eq!(rows(&mut place, Direction::Backward(1)), [5]);
        assert_eq!(rows(&mut place, Direction::Absolute(2)), [2]);
        assert_eq!(rows(&mut place, Direction::Absolute(9)), Vec::<u64>::new());
        assert_eq!(rows(&mut place, Direction::Absolute(-9)), Vec::<u64>::new());
        assert_eq!(place.at, 0);
        assert_eq!(rows(&mut place, Direction::Forward(ALL)), [1, 2, 3, 4, 5]);
        assert_eq!(rows(&mut place, Direction::Forward(0)), Vec::<u64>::new());
        let moved = place.fetch(5, true, Direction::Backward(ALL), true).unwrap();
        assert_eq!((moved.1, place.at), (5, 0));
        assert_eq!(place.fetch(5, true, Direction::Forward(9), true).unwrap().1, 5);
    }

    #[test]
    fn a_plan_that_can_run_backward_makes_a_scroll_cursor() {
        assert!(scrolls("select * from t where a::int > 1 order by a, b limit 3"));
        assert!(!scrolls("select a from t, lateral generate_series(1, 2)"));
        assert!(scrolls("values (1), (2)"));
        assert!(scrolls("select extract(year from d) from t"));
        assert!(!scrolls("select 1"));
        assert!(!scrolls("select count(*) from t"));
        assert!(!scrolls("select a from t join u using (a)"));
        assert!(!scrolls("select a from t group by a"));
        assert!(!scrolls("select a from t union select a from u"));
        assert_eq!(not_a_query("insert into t values (1)"), Some("insert"));
        assert_eq!(not_a_query(" (select 1)"), None);
        assert_eq!(not_a_query("SELECT 1"), None);
    }

    #[test]
    fn a_cursor_without_scroll_only_moves_forward() {
        let mut place = Place::default();
        assert_eq!(place.fetch(5, false, Direction::Absolute(1), false).unwrap().1, 1);
        assert_eq!(place.fetch(5, false, Direction::Absolute(3), false).unwrap().1, 1);
        assert_eq!(place.fetch(5, false, Direction::Absolute(1), false), Err(NoScroll));
        assert_eq!(place.fetch(5, false, Direction::Backward(0), false), Err(NoScroll));
    }
}
