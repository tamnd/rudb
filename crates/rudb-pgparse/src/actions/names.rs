//! The actions of the names and the constants: the qualified names, the function names, the
//! literal constants and the role names.

use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

impl rules::qualified_name for Parser<'_> {
    fn qualified_name_1(
        &mut self,
        v1: Option<Str>,
        at1: i32,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(Some(Box::new(makeRangeVar(None, v1, at1))))
    }

    fn qualified_name_2(
        &mut self,
        v1: Option<Str>,
        v2: List,
        at1: i32,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(Some(Box::new(makeRangeVarFromQualifiedName(v1, v2, at1, self)?)))
    }
}

impl rules::func_name for Parser<'_> {
    fn func_name_2(&mut self, v1: Option<Str>, v2: List) -> Result<List, Error> {
        check_func_name(lcons(Some(makeString(v1)), v2), self)
    }
}

impl rules::AexprConst for Parser<'_> {
    fn AexprConst_1(&mut self, v1: i32, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeIntConst(v1, at1)))
    }

    fn AexprConst_2(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeFloatConst(v1, at1)))
    }

    fn AexprConst_3(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeStringConst(v1, at1)))
    }

    fn AexprConst_4(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeBitStringConst(v1, at1)))
    }

    fn AexprConst_5(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Node>, Error> {
        // A hexadecimal string is a bit string constant, as in SQL99.
        Ok(Some(makeBitStringConst(v1, at1)))
    }

    fn AexprConst_6(
        &mut self,
        v1: List,
        v2: Option<Str>,
        at1: i32,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        // The `type 'literal'` syntax for any type.
        let t = TypeName { location: at1, ..makeTypeNameFromNameList(v1) };
        Ok(Some(makeStringConstCast(v2, at2, Some(Box::new(t)))))
    }

    #[allow(clippy::too_many_arguments)]
    fn AexprConst_7(
        &mut self,
        v1: List,
        v3: List,
        v4: List,
        v6: Option<Str>,
        at1: i32,
        at4: i32,
        at6: i32,
    ) -> Result<Option<Node>, Error> {
        // The same syntax with a type modifier. The rule uses `func_arg_list` and
        // `opt_sort_clause` to prevent reduce/reduce conflicts, but the names of the arguments and
        // `ORDER BY` are not allowed here.
        for arg in v3.iter().flatten() {
            if let Node::NamedArgExpr(arg) = arg {
                let message = "type modifier cannot have parameter name";
                return Err(self.error(ERRCODE_SYNTAX_ERROR, message, arg.location));
            }
        }
        if !v4.is_empty() {
            return Err(self.error(
                ERRCODE_SYNTAX_ERROR,
                "type modifier cannot have ORDER BY",
                at4,
            ));
        }
        let t = TypeName { typmods: v3, location: at1, ..makeTypeNameFromNameList(v1) };
        Ok(Some(makeStringConstCast(v6, at6, Some(Box::new(t)))))
    }

    fn AexprConst_8(
        &mut self,
        v1: Option<Box<TypeName>>,
        v2: Option<Str>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeStringConstCast(v2, at2, v1)))
    }

    fn AexprConst_9(
        &mut self,
        v1: Option<Box<TypeName>>,
        v2: Option<Str>,
        v3: List,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        let t = change(v1, |t| t.typmods = v3);
        Ok(Some(makeStringConstCast(v2, at2, t)))
    }

    fn AexprConst_10(
        &mut self,
        v1: Option<Box<TypeName>>,
        v3: i32,
        v5: Option<Str>,
        at3: i32,
        at5: i32,
    ) -> Result<Option<Node>, Error> {
        let typmods =
            list_make2(Some(makeIntConst(INTERVAL_FULL_RANGE, -1)), Some(makeIntConst(v3, at3)));
        let t = change(v1, |t| t.typmods = typmods);
        Ok(Some(makeStringConstCast(v5, at5, t)))
    }

    fn AexprConst_11(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeBoolAConst(true, at1)))
    }

    fn AexprConst_12(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeBoolAConst(false, at1)))
    }

    fn AexprConst_13(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeNullAConst(at1)))
    }
}

impl rules::SignedIconst for Parser<'_> {
    fn SignedIconst_2(&mut self, v2: i32) -> Result<i32, Error> {
        Ok(v2)
    }

    fn SignedIconst_3(&mut self, v2: i32) -> Result<i32, Error> {
        // The lexer gives an `Iconst` only for a value that fits in an `int4`, so the negation
        // cannot overflow.
        Ok(-v2)
    }
}

impl rules::RoleId for Parser<'_> {
    fn RoleId_1(&mut self, v1: Option<Box<RoleSpec>>, at1: i32) -> Result<Option<Str>, Error> {
        let spc = v1.map(|s| *s).unwrap_or_default();
        let message = match spc.roletype {
            RoleSpecType::ROLESPEC_CSTRING => return Ok(spc.rolename),
            RoleSpecType::ROLESPEC_PUBLIC => "role name \"public\" is reserved",
            RoleSpecType::ROLESPEC_SESSION_USER => {
                "SESSION_USER cannot be used as a role name here"
            }
            RoleSpecType::ROLESPEC_CURRENT_USER => {
                "CURRENT_USER cannot be used as a role name here"
            }
            RoleSpecType::ROLESPEC_CURRENT_ROLE => {
                "CURRENT_ROLE cannot be used as a role name here"
            }
            _ => return Ok(None),
        };
        Err(self.error(ERRCODE_RESERVED_NAME, message, at1))
    }
}

impl rules::RoleSpec for Parser<'_> {
    fn RoleSpec_1(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Box<RoleSpec>>, Error> {
        // `public` and `none` are not keywords, but they have a special meaning here.
        let n = match v1.as_deref() {
            Some("public") => makeRoleSpec(RoleSpecType::ROLESPEC_PUBLIC, at1),
            Some("none") => {
                return Err(self.error(
                    ERRCODE_RESERVED_NAME,
                    "role name \"none\" is reserved",
                    at1,
                ));
            }
            _ => RoleSpec { rolename: v1, ..makeRoleSpec(RoleSpecType::ROLESPEC_CSTRING, at1) },
        };
        Ok(Some(Box::new(n)))
    }

    fn RoleSpec_2(&mut self, at1: i32) -> Result<Option<Box<RoleSpec>>, Error> {
        Ok(Some(Box::new(makeRoleSpec(RoleSpecType::ROLESPEC_CURRENT_ROLE, at1))))
    }

    fn RoleSpec_3(&mut self, at1: i32) -> Result<Option<Box<RoleSpec>>, Error> {
        Ok(Some(Box::new(makeRoleSpec(RoleSpecType::ROLESPEC_CURRENT_USER, at1))))
    }

    fn RoleSpec_4(&mut self, at1: i32) -> Result<Option<Box<RoleSpec>>, Error> {
        Ok(Some(Box::new(makeRoleSpec(RoleSpecType::ROLESPEC_SESSION_USER, at1))))
    }
}
