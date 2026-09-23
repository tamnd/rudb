//! Binding struct literals and picking a field out of a struct.
//!
//! `{'a': 1, 'b': 'x'}` is `struct_pack(a := 1, b := 'x')` on the pin, a call whose type is a
//! STRUCT named by the call itself and not by the types of its arguments, so it is settled here and
//! not in the signature table, the same way `list_aggr` is. The plan records a call to
//! `struct_pack` whose type carries the names, and the kernel reads them off that type.
//!
//! `s.a`, `s['a']` and `struct_extract(s, 'a')` are one call too. The key has to be a constant,
//! since which field it names decides the type of the answer, so the binder checks it against the
//! struct's fields and the kernel finds the same field again by name.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;
use crate::fold;

/// The name the plan records for a struct literal, which is the one the kernel dispatches on.
pub(crate) const STRUCT_PACK: &str = "struct_pack";

/// The name the plan records for picking one field out of a struct.
pub(crate) const STRUCT_EXTRACT: &str = "struct_extract";

impl Binder<'_> {
    /// A struct built from bound values and the names they were written with.
    ///
    /// A name written twice is refused whatever its case, since a field is found without case on
    /// the pin and two of them could not be told apart. The sentence is the pin's, which names the
    /// second of the two.
    pub(crate) fn pack_struct(&mut self, names: &[String], values: &[ExprRef]) -> Result<ExprRef> {
        for (at, name) in names.iter().enumerate() {
            if names[..at].iter().any(|earlier| earlier.eq_ignore_ascii_case(name)) {
                return Err(Error::binder(format!(
                    "Duplicate named argument \"{name}\" in function call to '\"struct_pack\"'"
                )));
            }
        }
        if values.is_empty() {
            return Ok(self.add_constant(Value::Struct(Vec::new())));
        }
        let fields = names
            .iter()
            .zip(values)
            .map(|(name, &value)| Field::new(name.clone(), self.plan().expr_type(value).clone()))
            .collect();
        let args = self.plan_mut().add_expr_list(values);
        let recorded = self.plan_mut().intern(STRUCT_PACK);
        Ok(self.add_expr(Expr::Function { name: recorded, args }, LogicalType::Struct(fields)))
    }

    /// A bound call that picks a field out of a struct, or `None` when the call is not one.
    ///
    /// `struct_extract` is always one, and a subscript is one when what it subscripts is a struct,
    /// which is how `s['a']` and `s.a` reach the same place.
    pub(crate) fn struct_field(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        let extract = rudb_catalog::same_name(written, STRUCT_EXTRACT);
        let subscript = ["array_extract", "list_extract", "list_element"]
            .iter()
            .any(|name| rudb_catalog::same_name(written, name));
        let &[input, key] = bound else {
            return Ok(None);
        };
        let LogicalType::Struct(fields) = self.plan().expr_type(input).clone() else {
            return Ok(None);
        };
        if !extract && !subscript {
            return Ok(None);
        }
        let name = match fold::value_of(self.plan(), key) {
            Ok(Some(Value::Varchar(name))) => name,
            Ok(Some(value)) if value.logical_type().is_integer() => {
                return Err(Error::binder(
                    "struct_extract with an integer key can only be used on unnamed structs, use \
                     a string key instead",
                ));
            }
            _ => {
                return Err(Error::binder(
                    "Key name for struct_extract needs to be a constant string",
                ));
            }
        };
        let Some(at) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(&name))
        else {
            let entries: Vec<String> =
                fields.iter().map(|field| format!("\"{}\"", field.name)).collect();
            return Err(Error::binder(format!(
                "Could not find key \"{name}\" in struct\n\nCandidate Entries: {}",
                entries.join(", ")
            )));
        };
        let key = self.add_constant(Value::Varchar(fields[at].name.clone()));
        let args = self.plan_mut().add_expr_list(&[input, key]);
        let recorded = self.plan_mut().intern(STRUCT_EXTRACT);
        Ok(Some(self.add_expr(Expr::Function { name: recorded, args }, fields[at].ty.clone())))
    }
}
