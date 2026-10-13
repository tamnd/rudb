//! The JSON text of a value in a PostgreSQL session, as `datum_to_json_internal` in
//! `src/backend/utils/adt/json.c` writes it.
//!
//! The functions that build a `json` or a `jsonb` value, such as `to_json`, `row_to_json` and
//! `json_build_object`, take a value of any type and write it by the category of its type. A
//! boolean is `true` or `false`, a number is its output text, a date and a timestamp are strings
//! in the form of ISO 8601, a `json` or a `jsonb` value is its text, an array is a JSON array of
//! its elements and a row is a JSON object of its fields. Any other value is the string of its
//! output text. The kernels of `rudb_kernels::pgjsonbuild` write each category, and the kernel of
//! the function puts the pieces together.

use rudb_common::{Error, LogicalType, Result, SqlState, Value};
use rudb_kernels::pgjsonbuild::{ARRAY, LEAF, RECORD, TIME};
use rudb_plan::{ColumnBinding, Expr, ExprRef};

use crate::binder::Binder;
use crate::pgcalls::exact_oid;

impl Binder<'_> {
    /// The JSON text of `expr`, a `json` value that is null when `expr` is null.
    pub(crate) fn json_datum(&mut self, expr: ExprRef) -> Result<ExprRef> {
        use rudb_pgtypes::oid;
        let ty = self.plan().expr_type(expr).clone();
        match &ty {
            LogicalType::Null => return Ok(self.cast_to(expr, &LogicalType::Json)),
            LogicalType::List(element) => {
                let table = self.fresh_index();
                let name = self.plan_mut().intern("x");
                let params = self.plan_mut().add_name_list(&[name]);
                let element = (**element).clone();
                let value = self.add_expr(Expr::LambdaParam(ColumnBinding::new(table, 0)), element);
                let body = self.json_datum(value)?;
                let lambda = self.add_expr(Expr::Lambda { table, params, body }, LogicalType::Json);
                let args = self.plan_mut().add_expr_list(&[expr, lambda]);
                let transform = self.plan_mut().intern(crate::lambda::TRANSFORM);
                let texts = LogicalType::List(Box::new(LogicalType::Json));
                let texts = self.add_expr(Expr::Function { name: transform, args }, texts);
                return Ok(self.json_kernel(ARRAY, &[texts]));
            }
            LogicalType::Struct(fields) => {
                let fields = self.struct_fields(expr, fields);
                let mut names = Vec::with_capacity(fields.len());
                let mut texts = Vec::with_capacity(fields.len());
                for (field, name) in fields {
                    names.push(Value::Varchar(name));
                    texts.push(self.json_datum(field)?);
                }
                let names =
                    self.add_constant(Value::List { element: LogicalType::Varchar, values: names });
                let texts = match texts.is_empty() {
                    true => self.add_constant(Value::List {
                        element: LogicalType::Json,
                        values: Vec::new(),
                    }),
                    false => self.call("list_value", texts)?,
                };
                return Ok(self.json_kernel(RECORD, &[expr, names, texts]));
            }
            _ => {}
        }
        let (text, category) = match exact_oid(&ty) {
            Some(oid::BOOL) => (expr, "bool"),
            Some(
                oid
                @ (oid::INT2 | oid::INT4 | oid::INT8 | oid::FLOAT4 | oid::FLOAT8 | oid::NUMERIC),
            ) => (self.pg_output(expr, oid), "number"),
            Some(oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ) => {
                return Ok(self.json_kernel(TIME, &[expr]));
            }
            Some(oid::JSON | oid::JSONB) => (expr, "json"),
            _ => (self.output_text(expr)?, "string"),
        };
        let category = self.add_constant(Value::Varchar(category.into()));
        Ok(self.json_kernel(LEAF, &[text, category]))
    }

    /// Refuses a key of `json_build_object` that is not a scalar, as `datum_to_json_internal`
    /// refuses it with `key_scalar` set.
    pub(crate) fn json_key(&self, expr: ExprRef) -> Result<()> {
        match self.plan().expr_type(expr) {
            LogicalType::List(_)
            | LogicalType::Struct(_)
            | LogicalType::Json
            | LogicalType::Jsonb => {
                let message = "key value must be scalar, not array, composite, or json";
                Err(Error::binder(message)
                    .state(SqlState::INVALID_PARAMETER_VALUE)
                    .pg(message)
                    .unplaced())
            }
            _ => Ok(()),
        }
    }

    /// The call of the kernel `name`, which gives the JSON text of a value.
    fn json_kernel(&mut self, name: &str, args: &[ExprRef]) -> ExprRef {
        let name = self.plan_mut().intern(name);
        let args = self.plan_mut().add_expr_list(args);
        self.add_expr(Expr::Function { name, args }, LogicalType::Json)
    }
}
