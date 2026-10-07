//! The actions of the type names: `Typename` with its array bounds, the numeric, bit, character,
//! date and time, interval and JSON types, and the generic type names.

use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

/// `INTERVAL_MASK` of `timestamp.h`: the bit of an interval field in an interval typmod.
fn INTERVAL_MASK(b: i32) -> i32 {
    1 << b
}

/// `SystemTypeName(name)` with the location `location`.
fn system_type(name: &str, location: i32) -> Option<Box<TypeName>> {
    Some(Box::new(TypeName { location, ..SystemTypeName(name) }))
}

/// `SystemTypeName(name)` with the type modifiers `typmods` and the location `location`.
fn system_type_mods(name: &str, typmods: List, location: i32) -> Option<Box<TypeName>> {
    Some(Box::new(TypeName { typmods, location, ..SystemTypeName(name) }))
}

impl rules::Typename for Parser<'_> {
    fn Typename_1(
        &mut self,
        v1: Option<Box<TypeName>>,
        v2: List,
    ) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v1, |t| t.arrayBounds = v2))
    }

    fn Typename_2(
        &mut self,
        v2: Option<Box<TypeName>>,
        v3: List,
    ) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v2, |t| {
            t.arrayBounds = v3;
            t.setof = true;
        }))
    }

    fn Typename_3(
        &mut self,
        v1: Option<Box<TypeName>>,
        v4: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v1, |t| t.arrayBounds = list_make1(Some(makeInteger(v4)))))
    }

    fn Typename_4(
        &mut self,
        v2: Option<Box<TypeName>>,
        v5: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v2, |t| {
            t.arrayBounds = list_make1(Some(makeInteger(v5)));
            t.setof = true;
        }))
    }

    fn Typename_5(&mut self, v1: Option<Box<TypeName>>) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v1, |t| t.arrayBounds = list_make1(Some(makeInteger(-1)))))
    }

    fn Typename_6(&mut self, v2: Option<Box<TypeName>>) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v2, |t| {
            t.arrayBounds = list_make1(Some(makeInteger(-1)));
            t.setof = true;
        }))
    }
}

impl rules::opt_array_bounds for Parser<'_> {
    fn opt_array_bounds_1(&mut self, v1: List) -> Result<List, Error> {
        Ok(lappend(v1, Some(makeInteger(-1))))
    }

    fn opt_array_bounds_2(&mut self, v1: List, v3: i32) -> Result<List, Error> {
        Ok(lappend(v1, Some(makeInteger(v3))))
    }
}

impl rules::SimpleTypename for Parser<'_> {
    fn SimpleTypename_6(
        &mut self,
        v1: Option<Box<TypeName>>,
        v2: List,
    ) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v1, |t| t.typmods = v2))
    }

    fn SimpleTypename_7(
        &mut self,
        v1: Option<Box<TypeName>>,
        v3: i32,
        at3: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let typmods =
            list_make2(Some(makeIntConst(INTERVAL_FULL_RANGE, -1)), Some(makeIntConst(v3, at3)));
        Ok(change(v1, |t| t.typmods = typmods))
    }
}

impl rules::GenericType for Parser<'_> {
    fn GenericType_1(
        &mut self,
        v1: Option<Str>,
        v2: List,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let t = makeTypeName(v1.as_deref().unwrap_or_default());
        Ok(Some(Box::new(TypeName { typmods: v2, location: at1, ..t })))
    }

    fn GenericType_2(
        &mut self,
        v1: Option<Str>,
        v2: List,
        v3: List,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let t = makeTypeNameFromNameList(lcons(Some(makeString(v1)), v2));
        Ok(Some(Box::new(TypeName { typmods: v3, location: at1, ..t })))
    }
}

impl rules::Numeric for Parser<'_> {
    fn Numeric_1(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("int4", at1))
    }

    fn Numeric_2(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("int4", at1))
    }

    fn Numeric_3(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("int2", at1))
    }

    fn Numeric_4(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("int8", at1))
    }

    fn Numeric_5(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("float4", at1))
    }

    fn Numeric_6(
        &mut self,
        v2: Option<Box<TypeName>>,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v2, |t| t.location = at1))
    }

    fn Numeric_7(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("float8", at1))
    }

    fn Numeric_8(&mut self, v2: List, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type_mods("numeric", v2, at1))
    }

    fn Numeric_9(&mut self, v2: List, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type_mods("numeric", v2, at1))
    }

    fn Numeric_10(&mut self, v2: List, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type_mods("numeric", v2, at1))
    }

    fn Numeric_11(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("bool", at1))
    }
}

impl rules::opt_float for Parser<'_> {
    fn opt_float_1(&mut self, v2: i32, at2: i32) -> Result<Option<Box<TypeName>>, Error> {
        // The precision limits of the IEEE floating point types.
        let name = match v2 {
            ..1 => "precision for type float must be at least 1 bit",
            1..=24 => return Ok(Some(Box::new(SystemTypeName("float4")))),
            25..=53 => return Ok(Some(Box::new(SystemTypeName("float8")))),
            _ => "precision for type float must be less than 54 bits",
        };
        Err(self.error(ERRCODE_INVALID_PARAMETER_VALUE, name, at2))
    }

    fn opt_float_2(&mut self) -> Result<Option<Box<TypeName>>, Error> {
        Ok(Some(Box::new(SystemTypeName("float8"))))
    }
}

impl rules::ConstBit for Parser<'_> {
    fn ConstBit_2(&mut self, v1: Option<Box<TypeName>>) -> Result<Option<Box<TypeName>>, Error> {
        Ok(change(v1, |t| t.typmods = List::new()))
    }
}

impl rules::BitWithLength for Parser<'_> {
    fn BitWithLength_1(
        &mut self,
        v2: bool,
        v4: List,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let typname = if v2 { "varbit" } else { "bit" };
        Ok(system_type_mods(typname, v4, at1))
    }
}

impl rules::BitWithoutLength for Parser<'_> {
    fn BitWithoutLength_1(&mut self, v2: bool, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        // `bit` is `bit(1)`, and `varbit` has no limit.
        if v2 {
            Ok(system_type("varbit", at1))
        } else {
            Ok(system_type_mods("bit", list_make1(Some(makeIntConst(1, -1))), at1))
        }
    }
}

impl rules::ConstCharacter for Parser<'_> {
    fn ConstCharacter_2(
        &mut self,
        v1: Option<Box<TypeName>>,
    ) -> Result<Option<Box<TypeName>>, Error> {
        // With no length the type has no limit. In a column definition a `bpchar` with no length
        // is `bpchar(1)`, but a constant must not get that limit.
        Ok(change(v1, |t| t.typmods = List::new()))
    }
}

impl rules::CharacterWithLength for Parser<'_> {
    fn CharacterWithLength_1(
        &mut self,
        v1: Option<Str>,
        v3: i32,
        at1: i32,
        at3: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let typmods = list_make1(Some(makeIntConst(v3, at3)));
        Ok(system_type_mods(v1.as_deref().unwrap_or_default(), typmods, at1))
    }
}

impl rules::CharacterWithoutLength for Parser<'_> {
    fn CharacterWithoutLength_1(
        &mut self,
        v1: Option<Str>,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        // `char` is `char(1)`, and `varchar` has no limit.
        let name = v1.as_deref().unwrap_or_default();
        if name == "bpchar" {
            Ok(system_type_mods(name, list_make1(Some(makeIntConst(1, -1))), at1))
        } else {
            Ok(system_type(name, at1))
        }
    }
}

/// `$k ? "varchar" : "bpchar"`.
fn character(varying: bool) -> Option<Str> {
    Some(if varying { "varchar" } else { "bpchar" }.into())
}

impl rules::character for Parser<'_> {
    fn character_1(&mut self, v2: bool) -> Result<Option<Str>, Error> {
        Ok(character(v2))
    }

    fn character_2(&mut self, v2: bool) -> Result<Option<Str>, Error> {
        Ok(character(v2))
    }

    fn character_4(&mut self, v3: bool) -> Result<Option<Str>, Error> {
        Ok(character(v3))
    }

    fn character_5(&mut self, v3: bool) -> Result<Option<Str>, Error> {
        Ok(character(v3))
    }

    fn character_6(&mut self, v2: bool) -> Result<Option<Str>, Error> {
        Ok(character(v2))
    }
}

impl rules::ConstDatetime for Parser<'_> {
    fn ConstDatetime_1(
        &mut self,
        v3: i32,
        v5: bool,
        at1: i32,
        at3: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let name = if v5 { "timestamptz" } else { "timestamp" };
        Ok(system_type_mods(name, list_make1(Some(makeIntConst(v3, at3))), at1))
    }

    fn ConstDatetime_2(&mut self, v2: bool, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type(if v2 { "timestamptz" } else { "timestamp" }, at1))
    }

    fn ConstDatetime_3(
        &mut self,
        v3: i32,
        v5: bool,
        at1: i32,
        at3: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let name = if v5 { "timetz" } else { "time" };
        Ok(system_type_mods(name, list_make1(Some(makeIntConst(v3, at3))), at1))
    }

    fn ConstDatetime_4(&mut self, v2: bool, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type(if v2 { "timetz" } else { "time" }, at1))
    }
}

impl rules::ConstInterval for Parser<'_> {
    fn ConstInterval_1(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("interval", at1))
    }
}

/// `list_make1(makeIntConst(mask, location))`, the typmods of an interval with fields and no
/// precision.
fn interval(mask: i32, location: i32) -> List {
    list_make1(Some(makeIntConst(mask, location)))
}

/// `$$ = $3; linitial($$) = makeIntConst(mask, location);`: the fields of an interval that ends
/// in `SECOND`, where `interval_second` gave the mask of `SECOND` and maybe a precision.
fn interval_to_second(mut second: List, mask: i32, location: i32) -> List {
    if let Some(first) = second.first_mut() {
        *first = Some(makeIntConst(mask, location));
    }
    second
}

impl rules::opt_interval for Parser<'_> {
    fn opt_interval_1(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(YEAR), at1))
    }

    fn opt_interval_2(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(MONTH), at1))
    }

    fn opt_interval_3(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(DAY), at1))
    }

    fn opt_interval_4(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(HOUR), at1))
    }

    fn opt_interval_5(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(MINUTE), at1))
    }

    fn opt_interval_7(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(YEAR) | INTERVAL_MASK(MONTH), at1))
    }

    fn opt_interval_8(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(DAY) | INTERVAL_MASK(HOUR), at1))
    }

    fn opt_interval_9(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(DAY) | INTERVAL_MASK(HOUR) | INTERVAL_MASK(MINUTE), at1))
    }

    fn opt_interval_10(&mut self, v3: List, at1: i32) -> Result<List, Error> {
        let mask = INTERVAL_MASK(DAY)
            | INTERVAL_MASK(HOUR)
            | INTERVAL_MASK(MINUTE)
            | INTERVAL_MASK(SECOND);
        Ok(interval_to_second(v3, mask, at1))
    }

    fn opt_interval_11(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(HOUR) | INTERVAL_MASK(MINUTE), at1))
    }

    fn opt_interval_12(&mut self, v3: List, at1: i32) -> Result<List, Error> {
        let mask = INTERVAL_MASK(HOUR) | INTERVAL_MASK(MINUTE) | INTERVAL_MASK(SECOND);
        Ok(interval_to_second(v3, mask, at1))
    }

    fn opt_interval_13(&mut self, v3: List, at1: i32) -> Result<List, Error> {
        Ok(interval_to_second(v3, INTERVAL_MASK(MINUTE) | INTERVAL_MASK(SECOND), at1))
    }
}

impl rules::interval_second for Parser<'_> {
    fn interval_second_1(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(SECOND), at1))
    }

    fn interval_second_2(&mut self, v3: i32, at1: i32, at3: i32) -> Result<List, Error> {
        Ok(list_make2(Some(makeIntConst(INTERVAL_MASK(SECOND), at1)), Some(makeIntConst(v3, at3))))
    }
}

impl rules::JsonType for Parser<'_> {
    fn JsonType_1(&mut self, at1: i32) -> Result<Option<Box<TypeName>>, Error> {
        Ok(system_type("json", at1))
    }
}
