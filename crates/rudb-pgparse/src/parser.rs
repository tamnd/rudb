//! The LR parser that runs the generated tables.

use crate::generated::keywords::{self, Keyword};
use crate::generated::tables::{
    ACTION_BASE, CHARACTER, CHECK, DEFAULT_GOTO, DEFAULT_REDUCTION, ENTRIES, ERROR, FINAL,
    GOTO_BASE, NO_BASE, RULE_LENGTH, RULE_LHS, RULE_NAME, SYMBOL_NAME,
};

/// The token of an ASCII character that is a token by itself, such as `;` or `+`.
pub fn character(c: u8) -> Option<u16> {
    let token = *CHARACTER.get(usize::from(c))?;
    (token != 0).then_some(token)
}

/// The keyword that a word spells, with ASCII letters in any case, as `kwlist.h` has it.
pub fn keyword(word: &str) -> Option<&'static Keyword> {
    keywords::lookup(word.as_bytes())
}

/// The place where the grammar stops accepting the input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SyntaxError {
    /// The index of the token that the grammar does not accept. It is the number of tokens when
    /// the input ends too early.
    pub token: usize,
}

/// The name of a rule as `productions.txt` has it, for example `a_expr.17`, or `None` when there
/// is no such rule.
pub fn rule_name(rule: usize) -> Option<&'static str> {
    RULE_NAME.get(rule).copied()
}

/// The name of a symbol as `gram.y` has it, for example `IDENT`, `'+'` or `a_expr`, or `None`
/// when there is no such symbol. The tokens come first.
pub fn symbol_name(symbol: usize) -> Option<&'static str> {
    SYMBOL_NAME.get(symbol).copied()
}

/// The entry of the action row of a state for a token, or the default of the state.
fn action(state: usize, token: u16) -> i16 {
    let base = ACTION_BASE[state];
    if base != NO_BASE {
        let at = base + i32::from(token);
        if let Ok(at) = usize::try_from(at)
            && CHECK.get(at) == Some(&(token as i16))
        {
            return ENTRIES[at];
        }
    }
    match DEFAULT_REDUCTION[state] {
        0 => ERROR,
        rule => -(rule as i16),
    }
}

/// The state that a nonterminal goes to from a state.
fn goto(state: usize, nonterminal: usize) -> usize {
    let base = GOTO_BASE[nonterminal];
    if base != NO_BASE {
        let at = base + state as i32;
        if let Ok(at) = usize::try_from(at)
            && CHECK.get(at) == Some(&(state as i16))
        {
            return ENTRIES[at] as usize;
        }
    }
    usize::from(DEFAULT_GOTO[nonterminal])
}

/// Runs the grammar over a list of tokens. The list does not end with the end token; the end of
/// the slice is the end of the input.
pub fn recognize(tokens: &[u16]) -> Result<(), SyntaxError> {
    let mut stack: Vec<usize> = vec![0];
    let mut at = 0;
    loop {
        // The stack is never empty: a reduction pops the symbols of its right side, and the first
        // state is under all of them.
        let state = stack[stack.len() - 1];
        // The end of the input is token 0, `$end`.
        let token = tokens.get(at).copied().unwrap_or(0);
        let entry = action(state, token);
        if entry == ERROR {
            return Err(SyntaxError { token: at });
        }
        if entry > 0 {
            if entry == FINAL {
                return Ok(());
            }
            stack.push(entry as usize);
            at += 1;
            continue;
        }
        let rule = usize::from(entry.unsigned_abs());
        stack.truncate(stack.len() - usize::from(RULE_LENGTH[rule]));
        let state = stack[stack.len() - 1];
        stack.push(goto(state, usize::from(RULE_LHS[rule])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::*;

    fn c(ch: u8) -> u16 {
        character(ch).unwrap()
    }

    #[test]
    fn statements() {
        assert_eq!(recognize(&[]), Ok(()));
        assert_eq!(recognize(&[SELECT, ICONST]), Ok(()));
        assert_eq!(recognize(&[SELECT, ICONST, c(b'+'), ICONST, c(b';'), SELECT]), Ok(()));
        assert_eq!(recognize(&[SELECT, FROM, IDENT]), Ok(()));
        let create = [CREATE, TABLE, IDENT, c(b'('), IDENT, INT_P, c(b','), IDENT, IDENT, c(b')')];
        assert_eq!(recognize(&create), Ok(()));
        let not_in = [SELECT, IDENT, NOT_LA, IN_P, c(b'('), ICONST, c(b')')];
        assert_eq!(recognize(&not_in), Ok(()));
    }

    #[test]
    fn names() {
        assert_eq!(rule_name(1), Some("parse_toplevel.1"));
        assert_eq!(rule_name(crate::RULES), None);
        assert_eq!(symbol_name(usize::from(SELECT)), Some("SELECT"));
        assert_eq!(symbol_name(usize::from(c(b'+'))), Some("'+'"));
        assert_eq!(symbol_name(crate::TOKENS), Some("$accept"));
    }

    #[test]
    fn errors() {
        assert_eq!(recognize(&[SELECT, ICONST, c(b'+')]), Err(SyntaxError { token: 3 }));
        assert_eq!(recognize(&[SELECT, ICONST, ICONST]), Err(SyntaxError { token: 2 }));
        assert_eq!(recognize(&[CREATE, SELECT]), Err(SyntaxError { token: 1 }));
        // `a = b = c` is an error because `=` does not associate.
        let chain = [SELECT, IDENT, c(b'='), IDENT, c(b'='), IDENT];
        assert_eq!(recognize(&chain), Err(SyntaxError { token: 4 }));
    }

    #[test]
    fn keywords() {
        use crate::Category;
        use crate::generated::keywords::KEYWORDS;
        assert_eq!(
            keyword("SELECT").map(|k| (k.token, k.category)),
            Some((SELECT, Category::Reserved))
        );
        assert_eq!(keyword("abort").map(|k| k.token), Some(ABORT_P));
        assert_eq!(keyword("zone").map(|k| k.bare_label), Some(true));
        assert!(keyword("selectx").is_none());
        assert!(keyword("").is_none());
        for k in &KEYWORDS {
            assert!(std::ptr::eq(keyword(k.name).unwrap(), k));
            assert!(std::ptr::eq(keyword(&k.name.to_uppercase()).unwrap(), k));
        }
    }
}
