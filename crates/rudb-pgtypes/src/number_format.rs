//! `to_char` for the number types, and `to_number`.
//!
//! This is a port of the `NUM` part of `src/backend/utils/adt/formatting.c`: `NUMDesc_prepare`,
//! `NUM_processor`, `int_to_roman`, `roman_to_int`, and the functions `int4_to_char`,
//! `int8_to_char`, `numeric_to_char`, `float4_to_char`, `float8_to_char` and
//! `numeric_to_number`. The structure follows the C code, so that a difference against the server
//! can be found by reading the two side by side.
//!
//! A [`NumberTemplate`] is the parsed template. The caller keeps it while the template text does
//! not change, as the cache of the server does. A template error, such as `multiple decimal
//! points`, comes from [`NumberTemplate::parse`].
//!
//! The symbols of the locale are the ones of the C locale: `-` and `+` for the signs, `.` for
//! `D`, `,` for `G` and a space for `L`. The engine has no `lc_numeric` and no `lc_monetary`.
//!
//! The C code writes the result into a buffer and takes the text up to the first zero byte. A
//! template can make it write a zero byte, for example a `0` past the digits that a `float8`
//! keeps, and the result then ends there. [`NumProc::finish`] does the same.

use std::fmt::Write;

use rudb_common::SqlState;

use crate::error::TypeError;
use crate::number::is_space;
use crate::numeric::{Numeric, NumericSign, numeric_in, numeric_out, numeric_out_sci};

/// `MAX_ROMAN_LEN`: `MMMDCCCLXXXVIII` is the longest Roman numeral.
const MAX_ROMAN_LEN: usize = 15;
/// `FLT_DIG` and `DBL_DIG`: the decimal digits that a `float4` and a `float8` keep.
const FLT_DIG: usize = 6;
const DBL_DIG: usize = 15;
/// The symbols of the C locale, as `NUM_prepare_locale` sets them.
const NEGATIVE_SIGN: &[u8] = b"-";
const POSITIVE_SIGN: &[u8] = b"+";
const DECIMAL_POINT: &[u8] = b".";
const THOUSANDS_SEP: &[u8] = b",";
const CURRENCY_SYMBOL: &[u8] = b" ";

const RM1: [&str; 9] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
const RM10: [&str; 9] = ["X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
const RM100: [&str; 9] = ["C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];

/// A keyword of a number template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Comma,
    Dec,
    Zero,
    Nine,
    B,
    C,
    D,
    E,
    Fm,
    G,
    L,
    Mi,
    Pl,
    Pr,
    RnUpper,
    RnLower,
    Sg,
    Sp,
    S,
    ThUpper,
    ThLower,
    V,
}

/// `NUM_keywords`, in the order that `index_seq_search` tries them. Only the keywords that start
/// with the same character are tried, so `SG` and `SP` come before `S`.
const KEYWORDS: [(&str, Key); 36] = [
    (",", Key::Comma),
    (".", Key::Dec),
    ("0", Key::Zero),
    ("9", Key::Nine),
    ("B", Key::B),
    ("C", Key::C),
    ("D", Key::D),
    ("EEEE", Key::E),
    ("FM", Key::Fm),
    ("G", Key::G),
    ("L", Key::L),
    ("MI", Key::Mi),
    ("PL", Key::Pl),
    ("PR", Key::Pr),
    ("RN", Key::RnUpper),
    ("SG", Key::Sg),
    ("SP", Key::Sp),
    ("S", Key::S),
    ("TH", Key::ThUpper),
    ("V", Key::V),
    ("b", Key::B),
    ("c", Key::C),
    ("d", Key::D),
    ("eeee", Key::E),
    ("fm", Key::Fm),
    ("g", Key::G),
    ("l", Key::L),
    ("mi", Key::Mi),
    ("pl", Key::Pl),
    ("pr", Key::Pr),
    ("rn", Key::RnLower),
    ("sg", Key::Sg),
    ("sp", Key::Sp),
    ("s", Key::S),
    ("th", Key::ThLower),
    ("v", Key::V),
];

/// A node of the parsed template: a keyword, or a character that `to_char` copies and that
/// `to_number` skips one input character for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    Action(Key),
    Char(char),
}

const F_DECIMAL: u32 = 1 << 1;
const F_LDECIMAL: u32 = 1 << 2;
const F_ZERO: u32 = 1 << 3;
const F_BLANK: u32 = 1 << 4;
const F_FILLMODE: u32 = 1 << 5;
const F_LSIGN: u32 = 1 << 6;
const F_BRACKET: u32 = 1 << 7;
const F_MINUS: u32 = 1 << 8;
const F_PLUS: u32 = 1 << 9;
const F_ROMAN: u32 = 1 << 10;
const F_MULTI: u32 = 1 << 11;
const F_PLUS_POST: u32 = 1 << 12;
const F_MINUS_POST: u32 = 1 << 13;
const F_EEEE: u32 = 1 << 14;

/// `NUMDesc_lsign`: where the `S` of the template puts the sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LocaleSign {
    #[default]
    None,
    Pre,
    Post,
}

/// `NUMDesc`: what the keywords of the template ask for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NumDesc {
    /// The digits before the point.
    pre: i32,
    /// The digits after the point.
    post: i32,
    lsign: LocaleSign,
    flag: u32,
    pre_lsign_num: i32,
    /// The digits after `V`.
    multi: i32,
    /// The place of the first `0` before the point, from 1.
    zero_start: i32,
    /// The place of the last `0`.
    zero_end: i32,
}

impl NumDesc {
    fn is(&self, flag: u32) -> bool {
        self.flag & flag != 0
    }

    /// `NUMDesc_prepare`.
    fn prepare(&mut self, key: Key) -> Result<(), TypeError> {
        if self.is(F_EEEE) && key != Key::E {
            return Err(syntax("\"EEEE\" must be the last pattern used"));
        }
        match key {
            Key::Nine => {
                if self.is(F_BRACKET) {
                    return Err(syntax("\"9\" must be ahead of \"PR\""));
                }
                if self.is(F_MULTI) {
                    self.multi += 1;
                } else if self.is(F_DECIMAL) {
                    self.post += 1;
                } else {
                    self.pre += 1;
                }
            }
            Key::Zero => {
                if self.is(F_BRACKET) {
                    return Err(syntax("\"0\" must be ahead of \"PR\""));
                }
                if !self.is(F_ZERO) && !self.is(F_DECIMAL) {
                    self.flag |= F_ZERO;
                    self.zero_start = self.pre + 1;
                }
                if self.is(F_DECIMAL) {
                    self.post += 1;
                } else {
                    self.pre += 1;
                }
                self.zero_end = self.pre + self.post;
            }
            Key::B => {
                if self.pre == 0 && self.post == 0 && !self.is(F_ZERO) {
                    self.flag |= F_BLANK;
                }
            }
            Key::D | Key::Dec => {
                if key == Key::D {
                    self.flag |= F_LDECIMAL;
                }
                if self.is(F_DECIMAL) {
                    return Err(syntax("multiple decimal points"));
                }
                if self.is(F_MULTI) {
                    return Err(syntax("cannot use \"V\" and decimal point together"));
                }
                self.flag |= F_DECIMAL;
            }
            Key::Fm => self.flag |= F_FILLMODE,
            Key::S => {
                if self.is(F_LSIGN) {
                    return Err(syntax("cannot use \"S\" twice"));
                }
                if self.is(F_PLUS) || self.is(F_MINUS) || self.is(F_BRACKET) {
                    return Err(syntax(
                        "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together",
                    ));
                }
                if !self.is(F_DECIMAL) {
                    self.lsign = LocaleSign::Pre;
                    self.pre_lsign_num = self.pre;
                    self.flag |= F_LSIGN;
                } else if self.lsign == LocaleSign::None {
                    self.lsign = LocaleSign::Post;
                    self.flag |= F_LSIGN;
                }
            }
            Key::Mi => {
                if self.is(F_LSIGN) {
                    return Err(syntax("cannot use \"S\" and \"MI\" together"));
                }
                self.flag |= F_MINUS;
                if self.is(F_DECIMAL) {
                    self.flag |= F_MINUS_POST;
                }
            }
            Key::Pl => {
                if self.is(F_LSIGN) {
                    return Err(syntax("cannot use \"S\" and \"PL\" together"));
                }
                self.flag |= F_PLUS;
                if self.is(F_DECIMAL) {
                    self.flag |= F_PLUS_POST;
                }
            }
            Key::Sg => {
                if self.is(F_LSIGN) {
                    return Err(syntax("cannot use \"S\" and \"SG\" together"));
                }
                self.flag |= F_MINUS | F_PLUS;
            }
            Key::Pr => {
                if self.is(F_LSIGN) || self.is(F_PLUS) || self.is(F_MINUS) {
                    return Err(syntax(
                        "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                    ));
                }
                self.flag |= F_BRACKET;
            }
            Key::RnUpper | Key::RnLower => {
                if self.is(F_ROMAN) {
                    return Err(syntax("cannot use \"RN\" twice"));
                }
                self.flag |= F_ROMAN;
            }
            Key::V => {
                if self.is(F_DECIMAL) {
                    return Err(syntax("cannot use \"V\" and decimal point together"));
                }
                self.flag |= F_MULTI;
            }
            Key::E => {
                if self.is(F_EEEE) {
                    return Err(syntax("cannot use \"EEEE\" twice"));
                }
                let others = F_BLANK
                    | F_FILLMODE
                    | F_LSIGN
                    | F_BRACKET
                    | F_MINUS
                    | F_PLUS
                    | F_ROMAN
                    | F_MULTI;
                if self.is(others) {
                    let mut error = syntax("\"EEEE\" is incompatible with other formats");
                    error.detail = Some(
                        "\"EEEE\" may only be used together with digit and decimal point patterns."
                            .to_string(),
                    );
                    return Err(error);
                }
                self.flag |= F_EEEE;
            }
            Key::Comma | Key::C | Key::G | Key::L | Key::Sp | Key::ThUpper | Key::ThLower => {}
        }
        if self.is(F_ROMAN) && self.flag & !(F_ROMAN | F_FILLMODE) != 0 {
            let mut error = syntax("\"RN\" is incompatible with other formats");
            error.detail = Some("\"RN\" may only be used together with \"FM\".".to_string());
            return Err(error);
        }
        Ok(())
    }
}

fn syntax(message: &str) -> TypeError {
    TypeError::new(SqlState::SYNTAX_ERROR, message.to_string())
}

/// A parsed number template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberTemplate {
    nodes: Vec<Node>,
    desc: NumDesc,
    empty: bool,
}

impl NumberTemplate {
    /// `parse_format` with the keywords of the number functions, and `NUMDesc_prepare` for each
    /// keyword. Text in double quotes is copied, and a backslash in it quotes the next character.
    /// Outside the quotes, a backslash quotes only a double quote.
    pub fn parse(template: &str) -> Result<NumberTemplate, TypeError> {
        let mut nodes = Vec::with_capacity(template.len());
        let mut desc = NumDesc::default();
        let mut rest = template;
        while !rest.is_empty() {
            if let Some(&(name, key)) = KEYWORDS.iter().find(|(name, _)| rest.starts_with(name)) {
                nodes.push(Node::Action(key));
                desc.prepare(key)?;
                rest = &rest[name.len()..];
            } else if let Some(quoted) = rest.strip_prefix('"') {
                let mut chars = quoted.chars();
                rest = "";
                while let Some(mut c) = chars.next() {
                    if c == '"' {
                        rest = chars.as_str();
                        break;
                    }
                    if c == '\\'
                        && let Some(next) = chars.clone().next()
                    {
                        chars.next();
                        c = next;
                    }
                    nodes.push(Node::Char(c));
                }
            } else {
                // A backslash before a double quote is dropped.
                let quoted = rest.strip_prefix("\\\"").map_or(rest, |_| &rest[1..]);
                let mut chars = quoted.chars();
                if let Some(c) = chars.next() {
                    nodes.push(Node::Char(c));
                }
                rest = chars.as_str();
            }
        }
        Ok(NumberTemplate { nodes, desc, empty: template.is_empty() })
    }
}

/// `int4_to_char`.
pub fn int4_to_char(value: i32, template: &NumberTemplate) -> Result<String, TypeError> {
    let mut num = template.desc;
    if template.empty {
        return Ok(String::new());
    }
    if num.is(F_ROMAN) {
        return roman_template(template, num, value);
    }
    if num.is(F_EEEE) {
        // A `float8` holds each `int4` with no loss.
        return exponent_template(template, num, f64::from(value));
    }
    let mut value = value;
    if num.is(F_MULTI) {
        let range = || integer_range("integer");
        let multi = 10f64.powf(f64::from(num.multi)).round_ties_even();
        if !(-2147483648.0..2147483648.0).contains(&multi) {
            return Err(range());
        }
        value = value.checked_mul(multi as i32).ok_or_else(range)?;
        num.pre += num.multi;
    }
    integer_template(template, num, i64::from(value))
}

/// `int8_to_char`.
pub fn int8_to_char(value: i64, template: &NumberTemplate) -> Result<String, TypeError> {
    let mut num = template.desc;
    if template.empty {
        return Ok(String::new());
    }
    if num.is(F_ROMAN) {
        let value = i32::try_from(value).unwrap_or(i32::MAX);
        return roman_template(template, num, value);
    }
    if num.is(F_EEEE) {
        // Through `numeric`, so that no digit is lost.
        let mut text = Vec::new();
        numeric_out_sci(&Numeric::from_integer(i128::from(value)), num.post, &mut text);
        if text.first() != Some(&b'-') {
            text.insert(0, b' ');
        }
        return NumProc::to_char(&template.nodes, &mut num, text, 0, 0);
    }
    let mut value = value;
    if num.is(F_MULTI) {
        let range = || integer_range("bigint");
        let multi = 10f64.powf(f64::from(num.multi)).round_ties_even();
        if !(-9223372036854775808.0..9223372036854775808.0).contains(&multi) {
            return Err(range());
        }
        value = value.checked_mul(multi as i64).ok_or_else(range)?;
        num.pre += num.multi;
    }
    integer_template(template, num, value)
}

/// `numeric_to_char`.
pub fn numeric_to_char(value: &Numeric, template: &NumberTemplate) -> Result<String, TypeError> {
    let mut num = template.desc;
    if template.empty {
        return Ok(String::new());
    }
    if num.is(F_ROMAN) {
        // Rounded half away from zero. A value past the range of `int4` is `PG_INT32_MAX`.
        let value = value.to_integer("integer").ok().and_then(|value| i32::try_from(value).ok());
        return roman_template(template, num, value.unwrap_or(i32::MAX));
    }
    if num.is(F_EEEE) {
        let text = match value.sign() {
            NumericSign::NaN | NumericSign::Infinity | NumericSign::NegativeInfinity => {
                special_exponent(&num)
            }
            _ => {
                let mut text = Vec::new();
                numeric_out_sci(value, num.post, &mut text);
                if text.first() != Some(&b'-') {
                    text.insert(0, b' ');
                }
                text
            }
        };
        return NumProc::to_char(&template.nodes, &mut num, text, 0, 0);
    }
    let mut value = value.clone();
    if num.is(F_MULTI) {
        value = value.mul(&Numeric::power_of_ten(num.multi)?)?;
        num.pre += num.multi;
    }
    let mut text = Vec::new();
    numeric_out(&value.round(num.post)?, &mut text);
    decimal_template(template, num, text)
}

/// `float4_to_char`. The value is printed with at most `FLT_DIG` digits.
pub fn float4_to_char(value: f32, template: &NumberTemplate) -> Result<String, TypeError> {
    let mut num = template.desc;
    if template.empty {
        return Ok(String::new());
    }
    if num.is(F_ROMAN) {
        let value = value.round_ties_even();
        let fits = !value.is_nan() && (-2147483648.0..2147483648.0).contains(&value);
        return roman_template(template, num, if fits { value as i32 } else { i32::MAX });
    }
    if num.is(F_EEEE) {
        return exponent_template(template, num, f64::from(value));
    }
    let mut value = value;
    if num.is(F_MULTI) {
        let multi = 10f64.powf(f64::from(num.multi)) as f32;
        value *= multi;
        num.pre += num.multi;
    }
    float_template(template, num, f64::from(value), FLT_DIG)
}

/// `float8_to_char`. The value is printed with at most `DBL_DIG` digits.
pub fn float8_to_char(value: f64, template: &NumberTemplate) -> Result<String, TypeError> {
    let mut num = template.desc;
    if template.empty {
        return Ok(String::new());
    }
    if num.is(F_ROMAN) {
        let value = value.round_ties_even();
        let fits = !value.is_nan() && (-2147483648.0..2147483648.0).contains(&value);
        return roman_template(template, num, if fits { value as i32 } else { i32::MAX });
    }
    if num.is(F_EEEE) {
        return exponent_template(template, num, value);
    }
    let mut value = value;
    if num.is(F_MULTI) {
        value *= 10f64.powf(f64::from(num.multi));
        num.pre += num.multi;
    }
    float_template(template, num, value, DBL_DIG)
}

/// `numeric_to_number`: the digits that the template reads, as a `numeric` with the scale of the
/// digits that were read. `None` for an empty template.
pub fn to_number(input: &str, template: &NumberTemplate) -> Result<Option<Numeric>, TypeError> {
    if template.empty {
        return Ok(None);
    }
    let mut num = template.desc;
    let number = NumProc::from_char(&template.nodes, &mut num, input.as_bytes())?;
    // The number is ASCII: a sign or a space, digits and a point.
    let number = String::from_utf8_lossy(&number);
    let scale = num.post;
    let precision = num.pre + num.multi + scale;
    let result = numeric_in(&number, ((precision << 16) | scale) + 4)?;
    if num.is(F_MULTI) {
        return result.mul(&Numeric::power_of_ten(-num.multi)?).map(Some);
    }
    Ok(Some(result))
}

/// The error of `int4mul` and `dtoi4`, and of their `int8` forms.
fn integer_range(type_name: &str) -> TypeError {
    TypeError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("{type_name} out of range"))
}

/// The Roman numeral of `value`, in the template.
fn roman_template(
    template: &NumberTemplate,
    mut num: NumDesc,
    value: i32,
) -> Result<String, TypeError> {
    NumProc::to_char(&template.nodes, &mut num, int_to_roman(value), 0, 0)
}

/// `%+.*e` of a float, with a space for the plus sign, in the template. NaN and the infinities
/// are `#` in the shape of the template.
fn exponent_template(
    template: &NumberTemplate,
    mut num: NumDesc,
    value: f64,
) -> Result<String, TypeError> {
    let text = match value.is_finite() {
        true => {
            let mut text = printf_float(value, num.post as usize, true, true).into_bytes();
            if text[0] == b'+' {
                text[0] = b' ';
            }
            text
        }
        false => special_exponent(&num),
    };
    NumProc::to_char(&template.nodes, &mut num, text, 0, 0)
}

/// The `#` that stand for NaN or an infinity with `EEEE`: room for the sign, the point, the `e`,
/// the sign of the exponent and two digits of it.
fn special_exponent(num: &NumDesc) -> Vec<u8> {
    let mut text = vec![b'#'; (num.pre + num.post + 6) as usize];
    text[0] = b' ';
    text[num.pre as usize + 1] = b'.';
    text
}

/// The digits of an integer, with `post` zeros after the point, in the template.
fn integer_template(
    template: &NumberTemplate,
    mut num: NumDesc,
    value: i64,
) -> Result<String, TypeError> {
    let sign = if value < 0 { b'-' } else { b'+' };
    let mut text = value.unsigned_abs().to_string().into_bytes();
    let pre_len = text.len();
    if num.post > 0 {
        text.push(b'.');
        text.resize(text.len() + num.post as usize, b'0');
    }
    let (text, out_pre_spaces) = fit_digits(&num, text, pre_len);
    NumProc::to_char(&template.nodes, &mut num, text, out_pre_spaces, sign)
}

/// The text of a `numeric` that is rounded to the digits of the template, in the template.
fn decimal_template(
    template: &NumberTemplate,
    mut num: NumDesc,
    text: Vec<u8>,
) -> Result<String, TypeError> {
    let (sign, text) = match text.strip_prefix(b"-") {
        Some(digits) => (b'-', digits.to_vec()),
        None => (b'+', text),
    };
    let pre_len = text.iter().position(|&b| b == b'.').unwrap_or(text.len());
    let (text, out_pre_spaces) = fit_digits(&num, text, pre_len);
    NumProc::to_char(&template.nodes, &mut num, text, out_pre_spaces, sign)
}

/// `%.*f` of a float with no more than `digits` digits, in the template.
fn float_template(
    template: &NumberTemplate,
    mut num: NumDesc,
    value: f64,
    digits: usize,
) -> Result<String, TypeError> {
    let pre_len = printf_float(value.abs(), 0, false, false).len();
    if pre_len >= digits {
        num.post = 0;
    } else if pre_len + num.post as usize > digits {
        num.post = (digits - pre_len) as i32;
    }
    let text = printf_float(value, num.post as usize, false, false).into_bytes();
    decimal_template(template, num, text)
}

/// The spaces before the digits when the template has room for more digits, or `#` for each
/// place of the template when it has room for fewer.
fn fit_digits(num: &NumDesc, text: Vec<u8>, pre_len: usize) -> (Vec<u8>, i32) {
    let pre = num.pre as usize;
    if pre_len < pre {
        return (text, (pre - pre_len) as i32);
    }
    if pre_len > pre {
        let mut hashes = vec![b'#'; pre + num.post as usize + 1];
        hashes[pre] = b'.';
        return (hashes, 0);
    }
    (text, 0)
}

/// `int_to_roman`: upper case with no padding, or 15 `#` for a value outside 1 to 3999.
fn int_to_roman(number: i32) -> Vec<u8> {
    if !(1..=3999).contains(&number) {
        return vec![b'#'; MAX_ROMAN_LEN];
    }
    let mut result = String::new();
    let digits = number.to_string();
    for (place, digit) in (1..=digits.len()).rev().zip(digits.bytes()) {
        let Some(at) = (digit - b'0').checked_sub(1) else {
            continue;
        };
        let at = usize::from(at);
        match place {
            4 => result.extend(std::iter::repeat_n('M', at + 1)),
            3 => result.push_str(RM100[at]),
            2 => result.push_str(RM10[at]),
            _ => result.push_str(RM1[at]),
        }
    }
    result.into_bytes()
}

/// The `snprintf` of PostgreSQL for `%.*f`, or `%.*e` when `exponent`, with `%+` when
/// `force_sign`. It writes `NaN` and `Infinity` where the C library writes `nan` and `inf`, and it
/// writes at most 350 digits after the point and zeros after them.
fn printf_float(value: f64, precision: usize, exponent: bool, force_sign: bool) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    let mut text = String::new();
    if value.is_sign_negative() {
        text.push('-');
    } else if force_sign {
        text.push('+');
    }
    let value = value.abs();
    if value.is_infinite() {
        text.push_str("Infinity");
        return text;
    }
    let digits = precision.min(350);
    let zeros = "0".repeat(precision - digits);
    if exponent {
        // The C library writes the exponent with a sign and at least two digits.
        let written = format!("{value:.digits$e}");
        let (mantissa, power) = written.split_once('e').expect("the exponent form has an e");
        let power: i32 = power.parse().expect("the exponent is a number");
        let _ = write!(text, "{mantissa}{zeros}e{power:+03}");
    } else {
        let _ = write!(text, "{value:.digits$}{zeros}");
    }
    text
}

/// `get_th`: the ordinal suffix of the last digit of the number.
fn ordinal(number: &[u8], upper: bool) -> Result<&'static str, TypeError> {
    let Some(&last) = number.last().filter(|last| last.is_ascii_digit()) else {
        return Err(TypeError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("\"{}\" is not a number", String::from_utf8_lossy(number)),
        ));
    };
    // The teens all take `th`.
    let teen = number.len() > 1 && number[number.len() - 2] == b'1';
    let suffix = match (last, teen) {
        (b'1', false) => ["st", "ST"],
        (b'2', false) => ["nd", "ND"],
        (b'3', false) => ["rd", "RD"],
        _ => ["th", "TH"],
    };
    Ok(suffix[usize::from(upper)])
}

/// The length of the character at the start of `bytes`, as `pg_mblen_range` gives it in UTF-8.
fn char_len(bytes: &[u8]) -> usize {
    let len = match bytes.first() {
        Some(0xc0..0xe0) => 2,
        Some(0xe0..0xf0) => 3,
        Some(0xf0..) => 4,
        _ => 1,
    };
    len.min(bytes.len().max(1))
}

/// `NUMProc`: the state of `NUM_processor`. `number` is the text of the number, and `number_p`
/// the place in it. `to_char` writes `out`, and `to_number` reads `input` at `at`.
struct NumProc<'a> {
    num: &'a mut NumDesc,
    sign: u8,
    sign_wrote: bool,
    num_count: i32,
    num_in: bool,
    num_curr: i32,
    out_pre_spaces: i32,
    read_dec: bool,
    read_post: i32,
    read_pre: i32,
    number: Vec<u8>,
    number_p: usize,
    last_relevant: Option<usize>,
    out: Vec<u8>,
    input: &'a [u8],
    at: usize,
}

impl<'a> NumProc<'a> {
    fn new(num: &'a mut NumDesc, number: Vec<u8>, input: &'a [u8]) -> NumProc<'a> {
        if num.zero_start != 0 {
            num.zero_start -= 1;
        }
        NumProc {
            num,
            sign: 0,
            sign_wrote: false,
            num_count: 0,
            num_in: false,
            num_curr: 0,
            out_pre_spaces: 0,
            read_dec: false,
            read_post: 0,
            read_pre: 0,
            number,
            number_p: 0,
            last_relevant: None,
            out: Vec::new(),
            input,
            at: 0,
        }
    }

    /// The byte of the number at `at`, or 0 past its end, as the C string has.
    fn number_at(&self, at: usize) -> u8 {
        self.number.get(at).copied().unwrap_or(0)
    }

    /// `IS_PREDEC_SPACE`: a zero before the point that `9.9` writes as a space.
    fn predec_space(&self) -> bool {
        !self.num.is(F_ZERO)
            && self.number_p == 0
            && self.number_at(0) == b'0'
            && self.num.post != 0
    }

    fn last_relevant_is_point(&self) -> bool {
        self.last_relevant.is_some_and(|at| self.number_at(at) == b'.')
    }

    /// `NUM_processor` for `to_char`: writes `number` in the template.
    fn to_char(
        nodes: &[Node],
        num: &mut NumDesc,
        number: Vec<u8>,
        out_pre_spaces: i32,
        sign: u8,
    ) -> Result<String, TypeError> {
        let mut np = NumProc::new(num, number, &[]);
        if np.num.is(F_EEEE) {
            return Ok(np.finish(true));
        }
        np.sign = sign;
        // `MI`, `PL` and `SG` write the sign themselves.
        if np.num.is(F_PLUS) || np.num.is(F_MINUS) {
            np.sign_wrote = !(np.num.is(F_PLUS) && !np.num.is(F_MINUS));
        } else {
            if sign != b'-' && np.num.is(F_FILLMODE) {
                np.num.flag &= !F_BRACKET;
            }
            np.sign_wrote = sign == b'+' && np.num.is(F_FILLMODE) && !np.num.is(F_LSIGN);
            if np.num.lsign == LocaleSign::Pre && np.num.pre == np.num.pre_lsign_num {
                np.num.lsign = LocaleSign::Post;
            }
        }
        np.num_count = np.num.post + np.num.pre - 1;
        np.out_pre_spaces = out_pre_spaces;
        if np.num.is(F_FILLMODE) && np.num.is(F_DECIMAL) {
            np.last_relevant = last_relevant_decimal(&np.number);
            // The digits of a `0` are kept, but not past the end of the number, which a float
            // can make shorter than the template.
            if let Some(last) = np.last_relevant
                && np.num.zero_end > np.out_pre_spaces
            {
                let last_zero = (np.number.len().saturating_sub(1))
                    .min((np.num.zero_end - np.out_pre_spaces) as usize);
                if last < last_zero {
                    np.last_relevant = Some(last_zero);
                }
            }
        }
        if !np.sign_wrote && np.out_pre_spaces == 0 {
            np.num_count += 1;
        }

        for &node in nodes {
            let key = match node {
                Node::Char(c) => {
                    let mut buf = [0; 4];
                    np.out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    continue;
                }
                Node::Action(key) => key,
            };
            let fill = np.num.is(F_FILLMODE);
            match key {
                Key::Nine | Key::Zero | Key::Dec | Key::D => np.numpart_to_char(key),
                Key::Comma | Key::G => match np.num_in {
                    true => np.out.extend_from_slice(THOUSANDS_SEP),
                    false if fill => {}
                    false => np.out.push(b' '),
                },
                Key::L => np.out.extend_from_slice(CURRENCY_SYMBOL),
                Key::RnUpper | Key::RnLower => {
                    let mut roman = np.number[np.number_p.min(np.number.len())..].to_vec();
                    if key == Key::RnLower {
                        roman.make_ascii_lowercase();
                    }
                    if !fill {
                        let pad = MAX_ROMAN_LEN.saturating_sub(roman.len());
                        np.out.resize(np.out.len() + pad, b' ');
                    }
                    np.out.extend_from_slice(&roman);
                }
                Key::ThUpper | Key::ThLower => {
                    if np.num.is(F_ROMAN)
                        || np.number_at(0) == b'#'
                        || np.sign == b'-'
                        || np.num.is(F_DECIMAL)
                    {
                        continue;
                    }
                    let suffix = ordinal(&np.number, key == Key::ThUpper)?;
                    np.out.extend_from_slice(suffix.as_bytes());
                }
                Key::Mi => match np.sign {
                    b'-' => np.out.push(b'-'),
                    _ if fill => {}
                    _ => np.out.push(b' '),
                },
                Key::Pl => match np.sign {
                    b'+' => np.out.push(b'+'),
                    _ if fill => {}
                    _ => np.out.push(b' '),
                },
                Key::Sg => np.out.push(np.sign),
                Key::B | Key::C | Key::E | Key::Fm | Key::Pr | Key::Sp | Key::S | Key::V => {}
            }
        }
        Ok(np.finish(false))
    }

    /// The text that `to_char` wrote, up to the first zero byte, or the number for `EEEE`.
    fn finish(self, number: bool) -> String {
        let mut text = if number { self.number } else { self.out };
        if let Some(end) = text.iter().position(|&b| b == 0) {
            text.truncate(end);
        }
        String::from_utf8(text).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into())
    }

    /// `NUM_numpart_to_char`: writes the sign when it is due, then the digit, the zero, the
    /// space or the point for a `9`, `0`, `.` or `D`.
    fn numpart_to_char(&mut self, key: Key) {
        if self.num.is(F_ROMAN) {
            return;
        }
        let fill = self.num.is(F_FILLMODE);
        let zero = self.num.is(F_ZERO);
        self.num_in = false;
        // `IS_PREDEC_SPACE` makes `9.9` write `0.1` as ` .1`.
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces
                || (zero && self.num.zero_start == self.num_curr))
            && (!self.predec_space() || self.last_relevant_is_point())
        {
            if self.num.is(F_LSIGN) {
                if self.num.lsign == LocaleSign::Pre {
                    self.out.extend_from_slice(self.locale_sign());
                    self.sign_wrote = true;
                }
            } else if self.num.is(F_BRACKET) {
                self.out.push(if self.sign == b'+' { b' ' } else { b'<' });
                self.sign_wrote = true;
            } else if self.sign == b'+' {
                if !fill {
                    self.out.push(b' ');
                }
                self.sign_wrote = true;
            } else if self.sign == b'-' {
                self.out.push(b'-');
                self.sign_wrote = true;
            }
        }

        if self.num_curr < self.out_pre_spaces && (self.num.zero_start > self.num_curr || !zero) {
            if !fill {
                self.out.push(b' ');
            }
        } else if zero
            && self.num_curr < self.out_pre_spaces
            && self.num.zero_start <= self.num_curr
        {
            self.out.push(b'0');
            self.num_in = true;
        } else {
            let at = self.number_at(self.number_p);
            if at == b'.' {
                // The C code writes the point in each case, as `last_relevant` is only set with
                // `FM`. `FM9.9` writes `0` as `0.`, as Oracle does.
                self.out.extend_from_slice(DECIMAL_POINT);
            } else if self.last_relevant.is_some_and(|last| self.number_p > last)
                && key != Key::Zero
            {
                // A zero after the last relevant digit, which `FM` drops.
            } else if self.predec_space() {
                if !fill {
                    self.out.push(b' ');
                } else if self.last_relevant_is_point() {
                    self.out.push(b'0');
                }
            } else {
                // Past the end of the number this writes the zero byte, as the C code does.
                self.out.push(at);
                self.num_in = true;
            }
            if at != 0 {
                self.number_p += 1;
            }
        }

        let mut end = self.num_count
            + i32::from(self.out_pre_spaces != 0)
            + i32::from(self.num.is(F_DECIMAL));
        if self.last_relevant == Some(self.number_p) {
            end = self.num_curr;
        }
        if self.num_curr + 1 == end {
            if self.sign_wrote && self.num.is(F_BRACKET) {
                self.out.push(if self.sign == b'+' { b' ' } else { b'>' });
            } else if self.num.is(F_LSIGN) && self.num.lsign == LocaleSign::Post {
                self.out.extend_from_slice(self.locale_sign());
            }
        }
        self.num_curr += 1;
    }

    fn locale_sign(&self) -> &'static [u8] {
        if self.sign == b'-' { NEGATIVE_SIGN } else { POSITIVE_SIGN }
    }

    /// The input byte at `at`. The caller checks that the input has one.
    fn input_at(&self) -> u8 {
        self.input[self.at]
    }

    fn input_ended(&self) -> bool {
        self.at >= self.input.len()
    }

    fn input_starts_with(&self, symbol: &[u8]) -> bool {
        self.input[self.at.min(self.input.len())..].starts_with(symbol)
    }

    /// `NUM_processor` for `to_number`: the sign and the digits that the template reads from
    /// the input, as the text of a number. The first byte is the sign, or a space.
    fn from_char(nodes: &[Node], num: &mut NumDesc, input: &[u8]) -> Result<Vec<u8>, TypeError> {
        let mut np = NumProc::new(num, vec![b' '], input);
        if np.num.is(F_EEEE) {
            return Err(TypeError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "\"EEEE\" not supported for input".to_string(),
            ));
        }
        let fill = np.num.is(F_FILLMODE);
        // The loop moves to the next input byte at its end. An arm that moves by itself
        // continues.
        for &node in nodes {
            if np.input_ended() {
                break;
            }
            let key = match node {
                // Each character of the template skips one input character, whatever it is.
                Node::Char(_) => {
                    np.at += char_len(&input[np.at..]);
                    continue;
                }
                Node::Action(key) => key,
            };
            match key {
                Key::Nine | Key::Zero | Key::Dec | Key::D => np.numpart_from_char(key),
                Key::Comma => {
                    if fill || np.input_at() != b',' {
                        continue;
                    }
                }
                Key::G => {
                    if fill || !np.input_starts_with(THOUSANDS_SEP) {
                        continue;
                    }
                    np.at += THOUSANDS_SEP.len() - 1;
                }
                Key::L => {
                    np.eat_non_data(
                        std::str::from_utf8(CURRENCY_SYMBOL).map_or(1, |s| s.chars().count()),
                    );
                    continue;
                }
                Key::RnUpper | Key::RnLower => {
                    let Some(value) = np.roman_to_int() else {
                        return Err(TypeError::new(
                            SqlState::INVALID_TEXT_REPRESENTATION,
                            "invalid Roman numeral".to_string(),
                        ));
                    };
                    let digits = value.to_string();
                    np.number.extend_from_slice(digits.as_bytes());
                    np.num.pre = digits.len() as i32;
                    np.num.post = 0;
                    continue;
                }
                Key::ThUpper | Key::ThLower => {
                    if np.num.is(F_ROMAN) || np.number[0] == b'#' || np.num.is(F_DECIMAL) {
                        continue;
                    }
                    // Each form of `th` is two characters.
                    np.eat_non_data(2);
                    continue;
                }
                Key::Mi | Key::Pl | Key::Sg => {
                    let c = np.input_at();
                    let reads = match key {
                        Key::Mi => c == b'-',
                        Key::Pl => c == b'+',
                        _ => c == b'-' || c == b'+',
                    };
                    if !reads {
                        np.eat_non_data(1);
                        continue;
                    }
                    np.number[0] = c;
                }
                Key::B | Key::C | Key::E | Key::Fm | Key::Pr | Key::Sp | Key::S | Key::V => {
                    continue;
                }
            }
            np.at += 1;
        }
        // A point with no digit after it is dropped.
        if np.number.len() > 1 && np.number.last() == Some(&b'.') {
            np.number.pop();
        }
        np.num.post = np.read_post;
        Ok(np.number)
    }

    /// `NUM_numpart_from_char`: reads a sign before the first digit, then a digit or the point,
    /// then a sign after the last digit.
    fn numpart_from_char(&mut self, key: Key) {
        let mut isread = false;
        if self.input_ended() {
            return;
        }
        if self.input_at() == b' ' {
            self.at += 1;
        }
        if self.input_ended() {
            return;
        }

        if self.number[0] == b' '
            && matches!(key, Key::Zero | Key::Nine)
            && self.read_pre + self.read_post == 0
        {
            if self.num.is(F_LSIGN) && self.num.lsign == LocaleSign::Pre {
                if self.input_starts_with(NEGATIVE_SIGN) {
                    self.at += NEGATIVE_SIGN.len();
                    self.number[0] = b'-';
                } else if self.input_starts_with(POSITIVE_SIGN) {
                    self.at += POSITIVE_SIGN.len();
                    self.number[0] = b'+';
                }
            } else {
                let c = self.input_at();
                if c == b'-' || (self.num.is(F_BRACKET) && c == b'<') {
                    self.number[0] = b'-';
                    self.at += 1;
                } else if c == b'+' {
                    self.number[0] = b'+';
                    self.at += 1;
                }
            }
        }
        if self.input_ended() {
            return;
        }

        let c = self.input_at();
        if c.is_ascii_digit() {
            if self.read_dec && self.read_post == self.num.post {
                return;
            }
            self.number.push(c);
            if self.read_dec {
                self.read_post += 1;
            } else {
                self.read_pre += 1;
            }
            isread = true;
        } else if self.num.is(F_DECIMAL) && !self.read_dec && self.input_starts_with(DECIMAL_POINT)
        {
            self.at += DECIMAL_POINT.len() - 1;
            self.number.push(b'.');
            self.read_dec = true;
            isread = true;
        }
        if self.input_ended() {
            return;
        }

        // The place of a sign after the number is hard to know, so `FM9.999999MI` reads
        // `5.01-`, and `9.9S` reads `.5-`.
        if self.number[0] == b' ' && self.read_pre + self.read_post > 0 {
            if self.num.is(F_LSIGN)
                && isread
                && self.at + 1 < self.input.len()
                && !self.input[self.at + 1].is_ascii_digit()
            {
                // The loop moves past the last byte of the sign.
                let before = self.at;
                self.at += 1;
                if self.input_starts_with(NEGATIVE_SIGN) {
                    self.at += NEGATIVE_SIGN.len() - 1;
                    self.number[0] = b'-';
                } else if self.input_starts_with(POSITIVE_SIGN) {
                    self.at += POSITIVE_SIGN.len() - 1;
                    self.number[0] = b'+';
                }
                if self.number[0] == b' ' {
                    self.at = before;
                }
            } else if !isread
                && !self.num.is(F_LSIGN)
                && (self.num.is(F_PLUS) || self.num.is(F_MINUS))
            {
                // A sign that is not after a digit is read only without `S`, so that
                // `to_number('1 -', '9S')` does not read it.
                let c = self.input_at();
                if c == b'-' || c == b'+' {
                    self.number[0] = c;
                }
            }
        }
    }

    /// `NUM_eat_non_data_chars`: skips up to `n` input characters, but not a digit, a point, a
    /// comma or a sign.
    fn eat_non_data(&mut self, n: usize) {
        for _ in 0..n {
            if self.input_ended() || b"0123456789.,+-".contains(&self.input_at()) {
                break;
            }
            self.at += char_len(&self.input[self.at..]);
        }
    }

    /// `roman_to_int`: the value of the Roman numeral at the input, after any white space, or
    /// `None` when it is not a valid numeral.
    fn roman_to_int(&mut self) -> Option<i32> {
        fn value(c: u8) -> i32 {
            match c {
                b'I' => 1,
                b'V' => 5,
                b'X' => 10,
                b'L' => 50,
                b'C' => 100,
                b'D' => 500,
                b'M' => 1000,
                _ => 0,
            }
        }
        while !self.input_ended() && is_space(self.input_at()) {
            self.at += 1;
        }
        let mut numerals = Vec::with_capacity(MAX_ROMAN_LEN);
        while numerals.len() < MAX_ROMAN_LEN && !self.input_ended() {
            let c = self.input_at().to_ascii_uppercase();
            let v = value(c);
            if v == 0 {
                break;
            }
            numerals.push((c, v));
            self.at += 1;
        }
        if numerals.is_empty() {
            return None;
        }

        let mut result = 0;
        let mut repeat_count = 1;
        let (mut v_count, mut l_count, mut d_count) = (0, 0, 0);
        let mut subtracted: Option<i32> = None;
        // V, L and D come once, and nothing as large comes after them.
        let mut once = |c: u8, v: i32| -> bool {
            if (v_count > 0 && v >= 5) || (l_count > 0 && v >= 50) || (d_count > 0 && v >= 500) {
                return false;
            }
            match c {
                b'V' => v_count += 1,
                b'L' => l_count += 1,
                b'D' => d_count += 1,
                _ => {}
            }
            true
        };
        let mut i = 0;
        while i < numerals.len() {
            let (c, v) = numerals[i];
            // After a subtraction, no numeral is as large as the one that was subtracted.
            if subtracted.is_some_and(|last| v >= last) || !once(c, v) {
                return None;
            }
            let Some(&(next_c, next_v)) = numerals.get(i + 1) else {
                result += v;
                break;
            };
            if v < next_v {
                let valid = matches!(
                    (c, next_c),
                    (b'I', b'V' | b'X') | (b'X', b'L' | b'C') | (b'C', b'D' | b'M')
                );
                // A repeated numeral cannot be subtracted, as in `MCCM`.
                if !valid || repeat_count > 1 || !once(next_c, next_v) {
                    return None;
                }
                i += 1;
                repeat_count = 1;
                subtracted = Some(v);
                result += next_v - v;
            } else {
                if c == next_c {
                    repeat_count += 1;
                    if repeat_count > 3 {
                        return None;
                    }
                } else {
                    repeat_count = 1;
                }
                result += v;
            }
            i += 1;
        }
        Some(result)
    }
}

/// `get_last_relevant_decnum`: the place of the last digit after the point that is not zero, or
/// of the point when there is none. `None` when the number has no point.
fn last_relevant_decimal(number: &[u8]) -> Option<usize> {
    let point = number.iter().position(|&b| b == b'.')?;
    let last = number[point + 1..].iter().rposition(|&b| b != b'0');
    Some(last.map_or(point, |last| point + 1 + last))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The expected values are the output of PostgreSQL 19.

    fn template(text: &str) -> NumberTemplate {
        NumberTemplate::parse(text).unwrap()
    }

    fn int4(value: i32, text: &str) -> String {
        int4_to_char(value, &template(text)).unwrap()
    }

    fn numeric(value: &str, text: &str) -> String {
        numeric_to_char(&numeric_in(value, -1).unwrap(), &template(text)).unwrap()
    }

    fn float4(value: f32, text: &str) -> String {
        float4_to_char(value, &template(text)).unwrap()
    }

    fn float8(value: f64, text: &str) -> String {
        float8_to_char(value, &template(text)).unwrap()
    }

    fn number(input: &str, text: &str) -> String {
        let value = to_number(input, &template(text)).unwrap().unwrap();
        let mut out = Vec::new();
        numeric_out(&value, &mut out);
        String::from_utf8(out).unwrap()
    }

    fn error(result: Result<impl std::fmt::Debug, TypeError>) -> (String, String) {
        let error = result.unwrap_err();
        (error.sqlstate.as_str().to_string(), error.message)
    }

    fn template_error(text: &str) -> (String, String) {
        error(NumberTemplate::parse(text))
    }

    fn number_error(input: &str, text: &str) -> (String, String) {
        error(NumberTemplate::parse(text).and_then(|t| to_number(input, &t)))
    }

    fn pair(sqlstate: &str, message: &str) -> (String, String) {
        (sqlstate.to_string(), message.to_string())
    }

    #[test]
    fn an_integer_fills_the_digits_of_the_template() {
        assert_eq!(int4(1234, "9999"), " 1234");
        assert_eq!(int4(1234, "99"), " ##");
        assert_eq!(int4(-1234, "9999"), "-1234");
        assert_eq!(int4(0, "9999"), "    0");
        assert_eq!(int4(0, "0000"), " 0000");
        assert_eq!(int4(12, "0999"), " 0012");
        assert_eq!(int4(-12, "0999"), "-0012");
        assert_eq!(int4(12, "FM0999"), "0012");
        assert_eq!(int4(12, "9909"), "   12");
        assert_eq!(int4(12, "99.99"), " 12.00");
        assert_eq!(int4(1, "9990999"), "    0001");
        assert_eq!(int4(5, "00.00"), " 05.00");
        assert_eq!(int4(5, "FM00.00"), "05.00");
        assert_eq!(int4(5, "0.0"), " 5.0");
        assert_eq!(int4(-5, "00.00"), "-05.00");
        assert_eq!(int4(-5, "FM00.00"), "-05.00");
        assert_eq!(int4(12, "FM999."), "12");
        assert_eq!(int4(12, "999."), "  12");
        assert_eq!(int4(5, ".9"), " .#");
        assert_eq!(int4(0, "FM.99"), ".##");
    }

    #[test]
    fn a_numeric_is_rounded_to_the_template() {
        assert_eq!(numeric("12.345", "99.99"), " 12.35");
        assert_eq!(numeric("-12.345", "99.99"), "-12.35");
        assert_eq!(numeric("0.5", "9.9"), "  .5");
        assert_eq!(numeric("0.5", "0.9"), " 0.5");
        assert_eq!(numeric("0.05", "9.9"), "  .1");
        assert_eq!(numeric("0.5", "FM9.9"), ".5");
        assert_eq!(numeric("0", "FM9.9"), "0.");
        assert_eq!(numeric("12", "FM99.99"), "12.");
        assert_eq!(numeric("12.5", "FM99.90"), "12.50");
        assert_eq!(numeric("12.5", "FM9990.0000"), "12.5000");
        assert_eq!(numeric("1.5", "9"), " 2");
        assert_eq!(numeric("2.5", "9"), " 3");
        assert_eq!(numeric("-2.5", "9"), "-3");
        assert_eq!(numeric("0.125", "9.99"), "  .13");
        assert_eq!(numeric("NaN", "9999"), "  NaN");
        assert_eq!(numeric("Infinity", "9999"), " ####");
        assert_eq!(
            numeric("12345678901234567890.123", "99999999999999999999.999"),
            " 12345678901234567890.123"
        );
        assert_eq!(numeric("1e-20", "0.99999999999999999999999"), " 0.00000000000000000001000");
        assert_eq!(numeric("123.456", "FM999.999999"), "123.456");
        assert_eq!(numeric("-0.01", "9.9"), "  .0");
        assert_eq!(numeric("-0.01", "FM9.9"), "0.");
        assert_eq!(numeric("-0.01", "S9.9"), " +.0");
        assert_eq!(numeric("12.0", "FM999."), "12");
        assert_eq!(numeric("0.1", ".9"), " .#");
        assert_eq!(numeric("0.5", "FM.99"), ".##");
        assert_eq!(numeric("0.5", "00.0"), " 00.5");
    }

    #[test]
    fn group_separators_and_signs_go_where_the_template_puts_them() {
        assert_eq!(int4(1234567, "9,999,999"), " 1,234,567");
        assert_eq!(int4(1234567, "FM9,999,999"), "1,234,567");
        assert_eq!(int4(1234, "9,999,999"), "     1,234");
        assert_eq!(int4(1234, "FM9G999G999"), "1,234");
        assert_eq!(numeric("1234.5", "9G999D99"), " 1,234.50");
        assert_eq!(numeric("1234.5678", "9,999.99,99"), " 1,234.56,78");
        assert_eq!(numeric("1234.5678", "FM0,000.00,00"), "1,234.56,78");

        assert_eq!(int4(-12, "9999MI"), "  12-");
        assert_eq!(int4(12, "9999MI"), "  12 ");
        assert_eq!(int4(12, "FM9999MI"), "12");
        assert_eq!(int4(-12, "MI9999"), "-  12");
        assert_eq!(int4(12, "9999PL"), "   12+");
        assert_eq!(int4(-12, "PL9999"), "   -12");
        assert_eq!(int4(12, "SG9999"), "+  12");
        assert_eq!(int4(-12, "SG9999"), "-  12");
        assert_eq!(int4(12, "9999SG"), "  12+");
        assert_eq!(int4(-12, "9999PR"), "  <12>");
        assert_eq!(int4(12, "9999PR"), "   12 ");
        assert_eq!(int4(-12, "FM9999PR"), "<12>");
        assert_eq!(int4(12, "FM9999PR"), "12");
        assert_eq!(numeric("-12.5", "999.99PR"), " <12.50>");
        assert_eq!(int4(-12, "S9999"), "  -12");
        assert_eq!(int4(12, "S9999"), "  +12");
        assert_eq!(int4(12, "9999S"), "  12+");
        assert_eq!(int4(-12, "9999S"), "  12-");
        assert_eq!(numeric("-12.5", "99.9S"), "12.5-");
        assert_eq!(int4(12, "FMS9999"), "+12");
        assert_eq!(numeric("-0.5", "S9.9"), " -.5");
        assert_eq!(int4(-12, "FMS99"), "-12");
        assert_eq!(int4(12, "FMS99"), "+12");
        assert_eq!(int4(12, "FM99S"), "12+");
        assert_eq!(int4(-12, "FM99MI"), "12-");
        assert_eq!(int4(-12, "FMSG99"), "-12");
        assert_eq!(int4(12, "FMSG99"), "+12");
        assert_eq!(int4(12, "FMPL99"), "+12");
    }

    #[test]
    fn other_characters_are_copied() {
        assert_eq!(int4(12, ""), "");
        assert_eq!(int4(12, "abc"), "a");
        assert_eq!(int4(12, "\"num:\"999"), "num:  12");
        assert_eq!(int4(12, "9 9 9"), "   1 2");
        assert_eq!(int4(12, "L999"), "   12");
        assert_eq!(int4(12, "999 L"), "  12  ");
        assert_eq!(numeric("12.5", "99D9"), " 12.5");
        assert_eq!(int4(12, "B999"), "  12");
        assert_eq!(int4(12, "99th\"x\""), " 12thx");
        assert_eq!(float4(-12.5, "FM999.999"), "-12.5");
        assert_eq!(float8(0.0001, "FM0.9999"), "0.0001");
        assert_eq!(float8(1e15, "9999999999999999.9"), " 1000000000000000");
    }

    #[test]
    fn roman_numerals_and_ordinal_suffixes() {
        assert_eq!(int4(485, "RN"), "        CDLXXXV");
        assert_eq!(int4(485, "rn"), "        cdlxxxv");
        assert_eq!(int4(485, "FMRN"), "CDLXXXV");
        assert_eq!(int4(3999, "RN"), "      MMMCMXCIX");
        for value in [4000, 0, -5] {
            assert_eq!(int4(value, "RN"), "###############");
        }
        assert_eq!(numeric("2.5", "FMRN"), "III");
        assert_eq!(float8(2.5, "FMRN"), "II");
        assert_eq!(float4(3.5, "FMRN"), "IV");
        assert_eq!(int4(12, "RN9"), "            XII");
        assert_eq!(int4(12, "FMRN"), "XII");
        assert_eq!(int4(12, "RNFM"), "XII");

        assert_eq!(int4(1, "9th"), " 1st");
        assert_eq!(int4(2, "9TH"), " 2ND");
        assert_eq!(int4(3, "9th"), " 3rd");
        assert_eq!(int4(11, "99th"), " 11th");
        assert_eq!(int4(12, "99TH"), " 12TH");
        assert_eq!(int4(13, "99th"), " 13th");
        assert_eq!(int4(21, "99th"), " 21st");
        assert_eq!(int4(-1, "9th"), "-1");
        assert_eq!(numeric("1.5", "9.9th"), " 1.5");
        assert_eq!(int4(0, "9th"), " 0th");
        assert_eq!(numeric("12.3", "99.9th"), " 12.3");
        assert_eq!(int4(12, "FM99th"), "12th");
        assert_eq!(int4(12, "FM99TH"), "12TH");
        assert_eq!(int4(111, "999th"), " 111th");
        assert_eq!(int4(112, "999th"), " 112th");
        assert_eq!(int4(101, "999th"), " 101st");
        assert_eq!(
            error(float8_to_char(f64::NAN, &template("9999th"))),
            pair("22P02", "\"NaN\" is not a number")
        );
    }

    #[test]
    fn v_multiplies_by_a_power_of_ten() {
        assert_eq!(int4(12, "99V9"), " 120");
        assert_eq!(int4(12, "99V99"), " 1200");
        assert_eq!(numeric("12.45", "99V9"), " 125");
        assert_eq!(float8(12.45, "99V9"), " 124");
        assert_eq!(float4(12.45, "99V9"), " 124");
        assert_eq!(int4(123, "9V99"), " ###");
        assert_eq!(int4(12, "FM99V999"), "12000");
        assert_eq!(numeric("10.5", "9V9999999999"), " ###########");
        assert_eq!(float8(10.5, "9V9999999999"), " ###########");
        let range = |name: &str| pair("22003", &format!("{name} out of range"));
        assert_eq!(error(int4_to_char(1000000, &template("9V999999"))), range("integer"));
        assert_eq!(error(int4_to_char(10, &template("9V9999999999"))), range("integer"));
        assert_eq!(error(int8_to_char(10, &template("9V999999999999999999"))), range("bigint"));
    }

    #[test]
    fn eeee_writes_the_exponent_form() {
        assert_eq!(numeric("1234.5", "9.99EEEE"), " 1.23e+03");
        assert_eq!(numeric("-1234.5", "9.99EEEE"), "-1.23e+03");
        assert_eq!(numeric("0", "9.99EEEE"), " 0.00e+00");
        assert_eq!(numeric("9.99", "9.9EEEE"), " 10.0e+00");
        assert_eq!(numeric("NaN", "9.99EEEE"), " #.######");
        assert_eq!(int4(1234, "9.99EEEE"), " 1.23e+03");
        assert_eq!(int4(-1234, "9.99EEEE"), "-1.23e+03");
        assert_eq!(int8_to_char(1234, &template("9.99EEEE")).unwrap(), " 1.23e+03");
        assert_eq!(float8(1234.5, "9.99EEEE"), " 1.23e+03");
        assert_eq!(float4(1234.5, "9.99EEEE"), " 1.23e+03");
        assert_eq!(float8(f64::NAN, "9.99EEEE"), " #.######");
        assert_eq!(float8(f64::INFINITY, "99.99EEEE"), " ##.######");
        assert_eq!(float8(1e-300, "9.9EEEE"), " 1.0e-300");
        assert_eq!(float8(1e300, "9EEEE"), " 1e+300");
    }

    #[test]
    fn a_float_keeps_only_its_significant_digits() {
        assert_eq!(float8(123.456, "999.999"), " 123.456");
        assert_eq!(float4(123.456, "999.999"), " 123.456");
        assert_eq!(float8(1234567.891, "9999999.999"), " 1234567.891");
        assert_eq!(float8(123456789012345.6, "999999999999999.99"), " 123456789012346");
        assert_eq!(float8(0.1, "0.99999999999999999999"), " 0.10000000000000");
        assert_eq!(float8(1.5, "9"), " 2");
        assert_eq!(float8(2.5, "9"), " 2");
        assert_eq!(float8(-2.5, "9"), "-2");
        assert_eq!(float8(0.125, "9.99"), "  .12");
        assert_eq!(float8(f64::NAN, "9999"), "  NaN");
        assert_eq!(float8(f64::INFINITY, "9999"), " ####");
        assert_eq!(float8(f64::NEG_INFINITY, "9999"), "-####");
        assert_eq!(float8(f64::NAN, "99"), " ##");
        assert_eq!(float4(f32::NAN, "9999.99"), "  NaN");
        assert_eq!(float8(-0.0, "9.9"), " -.0");
        assert_eq!(float8(-0.01, "9.9"), " -.0");
        assert_eq!(float8(123.0, "999"), " 123");
        assert_eq!(float8(1.23456789e10, "99999999999.99"), " 12345678900.00");
        assert_eq!(float8(1.23456789e15, "9999999999999999.99"), " 1234567890000000");
        assert_eq!(float8(1.5e20, "999999999999999999999"), " 150000000000000000000");
        assert_eq!(float4(1.23456, "9.999999"), " 1.23456");
        assert_eq!(float4(123456.7, "999999.9"), " 123457");
        assert_eq!(float4(1234567.0, "9999999.9"), " 1234567");
    }

    #[test]
    fn a_wrong_template_is_a_syntax_error() {
        let syntax = |message: &str| pair("42601", message);
        assert_eq!(template_error("9.9.9"), syntax("multiple decimal points"));
        let v_and_point = syntax("cannot use \"V\" and decimal point together");
        assert_eq!(template_error("9V9.9"), v_and_point);
        assert_eq!(template_error("9.9V9"), v_and_point);
        assert_eq!(template_error("S9S"), syntax("cannot use \"S\" twice"));
        assert_eq!(template_error("S9MI"), syntax("cannot use \"S\" and \"MI\" together"));
        let s_and = syntax("cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together");
        assert_eq!(template_error("MI9S"), s_and);
        assert_eq!(template_error("9PRS"), s_and);
        assert_eq!(template_error("99PR9"), syntax("\"9\" must be ahead of \"PR\""));
        assert_eq!(template_error("99PR0"), syntax("\"0\" must be ahead of \"PR\""));
        assert_eq!(template_error("RNRN"), syntax("cannot use \"RN\" twice"));
        assert_eq!(template_error("9EEEE9"), syntax("\"EEEE\" must be the last pattern used"));
        assert_eq!(template_error("9EEEEEEEE"), syntax("cannot use \"EEEE\" twice"));
        let eeee = NumberTemplate::parse("FM9EEEE").unwrap_err();
        assert_eq!(eeee.message, "\"EEEE\" is incompatible with other formats");
        assert_eq!(
            eeee.detail.as_deref(),
            Some("\"EEEE\" may only be used together with digit and decimal point patterns.")
        );
        assert_eq!(template_error("S9EEEE").1, "\"EEEE\" is incompatible with other formats");
        assert_eq!(template_error("RN9.9").1, "\"RN\" is incompatible with other formats");
    }

    #[test]
    fn to_number_reads_the_digits_that_the_template_asks_for() {
        assert_eq!(number("1234", "9999"), "1234");
        assert_eq!(number("12,345.67", "99,999.99"), "12345.67");
        assert_eq!(number("-12.5", "99.9"), "-12.5");
        assert_eq!(number("12.5-", "99.9S"), "-12.5");
        assert_eq!(number("<12.5>", "99.9PR"), "-12.5");
        assert_eq!(number("12.5", "99.99"), "12.5");
        assert_eq!(number("$1,234.56", "L9,999.99"), "1234.56");
        assert_eq!(number("  1 2 3", "9 9 9"), "23");
        assert_eq!(number("1234", "99"), "12");
        assert_eq!(number("12", "9999"), "12");
        assert_eq!(number("1.234", "9.9"), "1.2");
        assert_eq!(number("1.234", "9.999999"), "1.234");
        assert_eq!(number("12345", "999V99"), "123.450000000000000000");
        assert_eq!(number("12", "9V9"), "1.20000000000000000");
        assert_eq!(number("-12", "S99"), "-12");
        assert_eq!(number("+12", "S99"), "12");
        assert_eq!(number("12-", "99MI"), "-12");
        assert_eq!(number("12+", "99PL"), "12");
        assert_eq!(number("-12", "SG99"), "-12");
        assert_eq!(number("CDLXXXV", "RN"), "485");
        assert_eq!(number("  xii", "rn"), "12");
        assert_eq!(number("MMMCMXCIX", "FMRN"), "3999");
        assert_eq!(number("12th", "99th"), "12");
        assert_eq!(number("1st", "9TH"), "1");
        assert_eq!(number("1,234", "9G999"), "1234");
        assert_eq!(number("1.234,5", "9G999D9"), "1.2");
        assert_eq!(number("12", "9,9"), "12");
        assert_eq!(number("1,2", "9,9"), "12");
        assert_eq!(number("1,2", "FM9,9"), "1");
        assert_eq!(number("12.", "99.9"), "12");
        assert_eq!(
            number("9999999999999999999999.99", "9999999999999999999999.99"),
            "9999999999999999999999.99"
        );
        assert_eq!(number("12", "0099"), "12");
        assert_eq!(number("0012", "9999"), "12");
        assert_eq!(number("12abc34", "99xxx99"), "1234");
        assert_eq!(number("12é34", "99x99"), "1234");
        assert_eq!(number("1 234", "9 999"), "1234");
        assert_eq!(number("1234", "9 999"), "134");
        assert_eq!(number("12 34", "9999"), "1234");
        assert_eq!(number(" -1", "S9"), "-1");
        assert_eq!(number("1 -", "9S"), "1");
        assert_eq!(number("1-", "9S"), "-1");
        assert_eq!(number("5.01-", "FM9.999999MI"), "-5.01");
        assert_eq!(number("123.001-", "FM9999.9999999S"), "-123.001");
        assert_eq!(number("1a2", "999"), "12");
        assert_eq!(number("12", "FM9999"), "12");
        assert_eq!(number(".5", ".9"), "0.5");
        assert_eq!(number("0.5", "0.9"), "0.5");
        assert_eq!(number("1,234,567", "9,999,999"), "1234567");
        assert_eq!(number("12-", "SG99"), "12");
        assert_eq!(number("-  12", "MI9999"), "-12");
        assert_eq!(number("12 ", "99PR"), "12");
        assert_eq!(number("x12", "L99"), "12");
        assert_eq!(to_number("12", &template("")).unwrap(), None);
    }

    #[test]
    fn to_number_errors() {
        let syntax = |text: &str| {
            pair("22P02", &format!("invalid input syntax for type numeric: \"{text}\""))
        };
        assert_eq!(number_error("", "9999"), syntax(" "));
        assert_eq!(number_error("", "RN"), syntax(" "));
        assert_eq!(number_error("-", "9"), syntax("-"));
        assert_eq!(number_error(".", "9.9"), syntax(" "));
        assert_eq!(number_error("- 1", "S9"), syntax("-"));
        assert_eq!(number_error("abc", "999"), syntax(" "));
        let roman = pair("22P02", "invalid Roman numeral");
        assert_eq!(number_error("IIII", "RN"), roman);
        assert_eq!(number_error("VV", "RN"), roman);
        let eeee = pair("0A000", "\"EEEE\" not supported for input");
        assert_eq!(number_error("12", "9.9EEEE"), eeee);
        assert_eq!(number_error("1e5", "9EEEE"), eeee);
        assert_eq!(number_error("12.34.5", "99.99.9"), pair("42601", "multiple decimal points"));
    }
}
