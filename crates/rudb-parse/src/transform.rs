//! From the parse tree to the AST.
//!
//! This is the one module that reads rule names out of the vendored grammar, and that is deliberate
//! containment: an upstream bump that renames a rule breaks a match arm here and nothing else in
//! the repository. `spec/04-architecture.md` section 4.5 says this transformer is ours and has to
//! be total over the rule table, and total is the load bearing word. Every rule reaches a defined
//! answer. For the ones this milestone covers that answer is an AST node, and for the rest it is a
//! `Not implemented` error naming the construct, which is what DuckDB itself answers for syntax it
//! parses and does not support. There is no arm that panics and none that silently drops a clause,
//! because a dropped clause is a wrong answer and a wrong answer is worse than an error.
//!
//! The mechanism that makes it tractable is the default arm. Two thirds of the parse tree is the
//! expression precedence chain, twenty rules of the form `X <- Y Tail*` that exist to make the
//! grammar unambiguous and that carry no meaning once it has been parsed. Rather than name all
//! twenty, the expression walker handles the case where a rule matched something interesting and
//! otherwise descends through any node with exactly one child. That is not a shortcut. It is the
//! statement that a rule with one child said nothing, which is true of every chain link, and it
//! means the twenty first precedence level upstream adds costs us nothing.

use std::collections::HashMap;

use rudb_common::{Error, Result};

use crate::ast::{
    Ast, BinaryOp, CaseArm, ColumnDef, CreateTable, CreateView, Distinct, DropTable, Expr, ExprRef,
    Insert, JoinKind, LiteralKind, Nulls, Order, OrderItem, Quantifier, Query, QueryBody, QueryRef,
    Select, SelectRef, SetOp, Slice, Source, SourceRef, Statement, StrRef, Target, UnaryOp,
};
use crate::generated::rules::PROGRAM;
use crate::matcher::{NONE, Tree, parse_tokens};
use crate::token::{Kind, Token};
use crate::tokenize::tokenize;

/// Parse a script and transform it into the AST.
///
/// The tokens are produced once and handed to both halves. Calling [`crate::parse`] here instead
/// would be shorter and would tokenize the query a second time, which `cargo xtask bench` prices
/// at about a tenth of the whole front end.
pub fn parse_ast(query: &str) -> Result<Ast> {
    let tokens = tokenize(query)?;
    let tree = parse_tokens(query, &tokens, PROGRAM, true)?;
    transform(query, &tokens, &tree)
}

/// Transform a parse tree that has already been produced.
pub fn transform(query: &str, tokens: &[Token], tree: &Tree) -> Result<Ast> {
    let mut transform = Transform {
        query,
        tokens,
        tree,
        ast: Ast::default(),
        interned: HashMap::new(),
        anonymous: 0,
    };
    transform.program(tree.root())?;
    Ok(transform.ast)
}

struct Transform<'a> {
    query: &'a str,
    tokens: &'a [Token],
    tree: &'a Tree,
    ast: Ast,
    interned: HashMap<String, StrRef>,
    /// How many bare `?` parameters have been seen, which is what numbers the next one.
    anonymous: u32,
}

impl<'a> Transform<'a> {
    // The parts that walk the parse tree without caring what it says.

    /// The text a node covers.
    fn text(&self, node: u32) -> &'a str {
        self.tree.text(node, self.query, self.tokens)
    }

    /// The name of the rule a node is.
    fn name(&self, node: u32) -> &'static str {
        self.tree.name(node)
    }

    /// The children of a node.
    ///
    /// Returned with the tree's lifetime rather than the borrow of `self`, so that the caller can
    /// iterate it while calling the `&mut self` methods that build the arena. Copying the `&Tree`
    /// out first is what buys that, and it is why every walker here starts by doing so.
    fn kids(&self, node: u32) -> impl Iterator<Item = u32> + use<'a> {
        let tree = self.tree;
        tree.children(node)
    }

    /// How many children a node has.
    fn count(&self, node: u32) -> usize {
        self.kids(node).count()
    }

    /// The n'th child, or `NONE`.
    fn nth(&self, node: u32, n: usize) -> u32 {
        self.kids(node).nth(n).unwrap_or(NONE)
    }

    /// The first child, or `NONE`.
    fn first(&self, node: u32) -> u32 {
        self.nth(node, 0)
    }

    /// The first child named `name`, or `NONE`.
    ///
    /// Optional parts of a sequence do not leave a placeholder behind, so `SimpleSelect` with a
    /// `WHERE` and no `GROUP BY` has the where clause as its second child and a `SimpleSelect` with
    /// neither has something else there. Positional indexing into an optional sequence is the
    /// single easiest way to write a transformer that is subtly wrong, so nothing here does it.
    fn find(&self, node: u32, name: &str) -> u32 {
        self.kids(node).find(|&kid| self.name(kid) == name).unwrap_or(NONE)
    }

    /// Every leaf of a subtree, in order.
    ///
    /// A leaf is a rule that matched only terminals, which for a name is the identifier itself. It
    /// is how all thirty odd spellings of a qualified name collapse into one walk: whether the
    /// parse said `SchemaQualification ReservedTableQualification ReservedColumnName` or
    /// `IdentifierDot IdentifierDot ColumnName`, the leaves are the parts in order.
    fn leaves(&self, node: u32, out: &mut Vec<u32>) {
        let mut any = false;
        for kid in self.kids(node) {
            any = true;
            self.leaves(kid, &mut *out);
        }
        if !any {
            out.push(node);
        }
    }

    // The parts that build the arena.

    /// Intern a string, returning its index.
    fn intern(&mut self, text: &str) -> StrRef {
        if let Some(&index) = self.interned.get(text) {
            return index;
        }
        let index = u32::try_from(self.ast.strings.len())
            .map_err(|_| Error::internal("more than four billion strings in one query"))
            .unwrap_or(NONE);
        self.ast.strings.push(text.to_string());
        self.interned.insert(text.to_string(), index);
        index
    }

    /// Push an expression and return its index.
    fn push(&mut self, expr: Expr) -> ExprRef {
        let index = self.ast.exprs.len() as u32;
        self.ast.exprs.push(expr);
        index
    }

    /// Push a from item and return its index.
    fn push_source(&mut self, source: Source) -> SourceRef {
        let index = self.ast.sources.len() as u32;
        self.ast.sources.push(source);
        index
    }

    /// Push a query and return its index.
    fn push_query(&mut self, query: Query) -> QueryRef {
        let index = self.ast.queries.len() as u32;
        self.ast.queries.push(query);
        index
    }

    /// Push a select and return its index.
    fn push_select(&mut self, select: Select) -> SelectRef {
        let index = self.ast.selects.len() as u32;
        self.ast.selects.push(select);
        index
    }

    /// Turn a vector of expressions into a slice of the expression list arena.
    fn expr_slice(&mut self, items: Vec<ExprRef>) -> Slice {
        let start = self.ast.expr_lists.len() as u32;
        self.ast.expr_lists.extend(items);
        Slice { start, len: self.ast.expr_lists.len() as u32 - start }
    }

    /// Turn a vector of strings into a slice of the name arena.
    fn part_slice(&mut self, items: Vec<StrRef>) -> Slice {
        let start = self.ast.parts.len() as u32;
        self.ast.parts.extend(items);
        Slice { start, len: self.ast.parts.len() as u32 - start }
    }

    /// Turn a vector of column definitions into a slice of the column arena.
    fn column_def_slice(&mut self, items: Vec<ColumnDef>) -> Slice {
        let start = self.ast.column_defs.len() as u32;
        self.ast.column_defs.extend(items);
        Slice { start, len: self.ast.column_defs.len() as u32 - start }
    }

    /// Turn a vector of qualified names into a slice of the name list arena.
    fn name_list_slice(&mut self, items: Vec<Slice>) -> Slice {
        let start = self.ast.name_lists.len() as u32;
        self.ast.name_lists.extend(items);
        Slice { start, len: self.ast.name_lists.len() as u32 - start }
    }

    /// The error for a construct the transformer does not cover yet.
    ///
    /// Both halves matter. The text is what the user wrote, which is the only part they can act on,
    /// and the rule name is what we act on, because it is the exact grammar rule to go implement.
    fn unsupported<T>(&self, node: u32) -> Result<T> {
        let text = self.text(node);
        let text = if text.chars().count() > 60 {
            let cut = text.char_indices().nth(60).map_or(text.len(), |(at, _)| at);
            format!("{}...", &text[..cut])
        } else {
            text.to_string()
        };
        Err(Error::not_implemented(format!(
            "{text} is not supported yet, the grammar rule is {}",
            self.name(node)
        )))
    }

    // Names.

    /// One identifier out of a subtree, with the quoting and any trailing dot removed.
    fn identifier(&mut self, node: u32) -> StrRef {
        let mut leaves = Vec::new();
        self.leaves(node, &mut leaves);
        let text = leaves.last().map_or("", |&leaf| self.text(leaf));
        let text = unquote(text.strip_suffix('.').unwrap_or(text));
        self.intern(&text)
    }

    /// Every part of a qualified name, outermost first.
    fn name_parts(&mut self, node: u32) -> Slice {
        let mut leaves = Vec::new();
        self.leaves(node, &mut leaves);
        let mut parts = Vec::with_capacity(leaves.len());
        for leaf in leaves {
            let text = self.text(leaf);
            // A node that covers no tokens is an optional part that was not written, and a bare
            // `*` is the star and not a name part. Neither is a component of anything.
            if text.is_empty() || text == "*" {
                continue;
            }
            let text = unquote(text.strip_suffix('.').unwrap_or(text));
            let interned = self.intern(&text);
            parts.push(interned);
        }
        self.part_slice(parts)
    }

    // Statements.

    /// `Program <- TopLevelStatement*`.
    fn program(&mut self, node: u32) -> Result<()> {
        for top in self.kids(node) {
            // A script that ends in a semicolon produces a last `TopLevelStatement` whose only
            // child is the end of input, because the grammar says `Statement? (';'+ / EndOfInput)`
            // and both halves of that are happy to match nothing. It is a real node and it is not a
            // statement, so it is dropped here rather than pretended away in the matcher.
            let Some(statement) = self.kids(top).find(|&kid| self.name(kid) == "Statement") else {
                continue;
            };
            let statement = self.statement(statement)?;
            self.ast.statements.push(statement);
        }
        Ok(())
    }

    /// `Statement <- SelectStatement / ...`, twenty seven alternatives of which four are done.
    fn statement(&mut self, node: u32) -> Result<Statement> {
        let inner = self.first(node);
        match self.name(inner) {
            "SelectStatement" => {
                let query = self.query(self.first(inner))?;
                Ok(Statement::Query(query))
            }
            "CreateStatement" => self.create_statement(inner),
            "DropStatement" => self.drop_statement(inner),
            "InsertStatement" => self.insert_statement(inner),
            _ => self.unsupported(inner),
        }
    }

    /// `CreateStatement <- 'CREATE' OrReplace? Temporary? CreateStatementVariation`.
    ///
    /// Of the nine variations, `CreateTableStmt` and `CreateViewStmt` are the ones that are done.
    /// The other seven are a macro, a sequence, a type, a schema, an index, a secret and a trigger,
    /// and each of them is a catalog entry this database has no room for yet.
    fn create_statement(&mut self, node: u32) -> Result<Statement> {
        let or_replace = self.find(node, "OrReplace") != NONE;
        let temporary = self.find(node, "Temporary") != NONE;
        let variation = self.find(node, "CreateStatementVariation");
        let inner = self.first(variation);
        // duckdb refuses this pair in the parser, with a caret under the `NOT`, because none of its
        // create rules has room for both. The vendored grammar has room for both, so the refusal is
        // here instead, which is the same stage and therefore the same sentence.
        if or_replace && self.find(inner, "IfNotExists") != NONE {
            return Err(Error::parser(
                "Cannot specify both OR REPLACE and IF NOT EXISTS within single create statement",
            ));
        }
        match self.name(inner) {
            "CreateTableStmt" => self.create_table_statement(inner, or_replace, temporary),
            "CreateViewStmt" => self.create_view_statement(inner, or_replace, temporary),
            _ => self.unsupported(inner),
        }
    }

    /// `CreateTableStmt <- 'TABLE' IfNotExists? QualifiedName CreateTableDefinition`.
    fn create_table_statement(
        &mut self,
        inner: u32,
        or_replace: bool,
        temporary: bool,
    ) -> Result<Statement> {
        let name = self.name_parts(self.find(inner, "QualifiedName"));
        let if_not_exists = self.find(inner, "IfNotExists") != NONE;
        let definition = self.find(inner, "CreateTableDefinition");
        let body = self.first(definition);
        let (columns, query) = match self.name(body) {
            "CreateColumnList" => (self.column_list(body)?, NONE),
            "CreateTableAs" => self.create_table_as(body)?,
            _ => return self.unsupported(body),
        };
        let index = self.ast.create_tables.len() as u32;
        self.ast.create_tables.push(CreateTable {
            name,
            columns,
            query,
            if_not_exists,
            or_replace,
            temporary,
        });
        Ok(Statement::CreateTable(index))
    }

    /// `CreateViewStmt <- CreateSecure? CreateRecursive? 'VIEW' IfNotExists? QualifiedName
    /// InsertColumnList? WithList? 'AS' SelectStatementInternal`.
    ///
    /// The body is transformed here as well as kept as text. Transforming it is what makes a view
    /// whose body does not parse a parse error at creation, which is where it belongs, and the text
    /// is what the catalog keeps so that the body can be bound again at every reference.
    fn create_view_statement(
        &mut self,
        inner: u32,
        or_replace: bool,
        temporary: bool,
    ) -> Result<Statement> {
        for kid in self.kids(inner) {
            // `SECURE` is a column and row policy, `RECURSIVE` is a different shape of view
            // entirely, and `WITH` carries options. Dropping any of the three silently would make a
            // view that is not the view that was asked for.
            if matches!(self.name(kid), "CreateSecure" | "CreateRecursive" | "WithList") {
                return self.unsupported(kid);
            }
        }
        let name = self.name_parts(self.find(inner, "QualifiedName"));
        let if_not_exists = self.find(inner, "IfNotExists") != NONE;
        let list = self.find(inner, "InsertColumnList");
        let columns = if list == NONE {
            Slice::default()
        } else {
            let mut parts = Vec::new();
            for kid in self.kids(self.find(list, "ColumnList")) {
                parts.push(self.identifier(kid));
            }
            self.part_slice(parts)
        };
        let body = self.find(inner, "SelectStatementInternal");
        let sql = self.text(body).to_string();
        let sql = self.intern(&sql);
        let query = self.query(body)?;
        let index = self.ast.create_views.len() as u32;
        self.ast.create_views.push(CreateView {
            name,
            columns,
            query,
            sql,
            if_not_exists,
            or_replace,
            temporary,
        });
        Ok(Statement::CreateView(index))
    }

    /// `CreateColumnList <- Parens(CreateTableColumnList?) PartitionSortedOptions? WithList?`.
    fn column_list(&mut self, node: u32) -> Result<Slice> {
        for kid in self.kids(node) {
            if matches!(self.name(kid), "PartitionOptions" | "SortedOptions" | "WithList") {
                return self.unsupported(kid);
            }
        }
        let list = self.find(node, "CreateTableColumnList");
        if list == NONE {
            // `CREATE TABLE t ()` parses. It is a table of no columns, and the catalog is entitled
            // to refuse it, but that is not this layer's refusal to make.
            return Ok(Slice::default());
        }
        let mut defs = Vec::new();
        for element in self.kids(list) {
            let inner = self.first(element);
            if self.name(inner) != "CreateTableColumnDefinition" {
                // A table level `PRIMARY KEY`, `UNIQUE`, `CHECK` or `FOREIGN KEY`. Constraints are
                // not enforced anywhere yet and silently dropping one is a wrong answer waiting to
                // happen, so it is refused instead.
                return self.unsupported(inner);
            }
            defs.push(self.column_definition(self.first(inner))?);
        }
        Ok(self.column_def_slice(defs))
    }

    /// `ColumnDefinition <- DottedIdentifier Type? GeneratedColumn? ConstraintNameClause?
    /// ColumnConstraint*`.
    fn column_definition(&mut self, node: u32) -> Result<ColumnDef> {
        let name = self.identifier(self.find(node, "DottedIdentifier"));
        let type_node = self.find(node, "Type");
        let ty = if type_node == NONE {
            NONE
        } else {
            let text = self.text(type_node).to_string();
            self.intern(&text)
        };
        if self.find(node, "GeneratedColumn") != NONE {
            return self.unsupported(self.find(node, "GeneratedColumn"));
        }
        let mut not_null = false;
        for kid in self.kids(node) {
            if self.name(kid) != "ColumnConstraint" {
                continue;
            }
            let constraint = self.first(kid);
            match self.name(constraint) {
                "NotNullConstraint" => {
                    not_null = self.name(self.first(constraint)) == "NotNullColumnConstraint";
                }
                _ => return self.unsupported(constraint),
            }
        }
        Ok(ColumnDef { name, ty, not_null })
    }

    /// `CreateTableAs <- IdentifierList? PartitionSortedOptions? WithList? 'AS' Statement
    /// WithData?`.
    ///
    /// The names in the `IdentifierList` become column definitions with no type, because the types
    /// are the query's and only the names are the syntax's to say.
    fn create_table_as(&mut self, node: u32) -> Result<(Slice, QueryRef)> {
        for kid in self.kids(node) {
            if matches!(
                self.name(kid),
                "PartitionOptions" | "SortedOptions" | "WithList" | "WithData"
            ) {
                return self.unsupported(kid);
            }
        }
        let names = self.find(node, "IdentifierList");
        let columns = if names == NONE {
            Slice::default()
        } else {
            let mut defs = Vec::new();
            for kid in self.kids(names) {
                let name = self.identifier(kid);
                defs.push(ColumnDef { name, ty: NONE, not_null: false });
            }
            self.column_def_slice(defs)
        };
        let statement = self.find(node, "Statement");
        let inner = self.first(statement);
        if self.name(inner) != "SelectStatement" {
            return self.unsupported(inner);
        }
        let query = self.query(self.first(inner))?;
        Ok((columns, query))
    }

    /// `DropStatement <- 'DROP' DropEntries DropBehavior?`.
    ///
    /// `DropTable <- TableOrView IfExists? List(BaseTableName)`, and `TableOrView` covers `VIEW`
    /// and `MATERIALIZED VIEW` as well as `TABLE`, so it is checked rather than assumed. The first
    /// two are done and a materialized view is not a thing this database has.
    fn drop_statement(&mut self, node: u32) -> Result<Statement> {
        if self.find(node, "DropBehavior") != NONE {
            return self.unsupported(self.find(node, "DropBehavior"));
        }
        let entries = self.find(node, "DropEntries");
        let inner = self.first(entries);
        if self.name(inner) != "DropTable" {
            return self.unsupported(inner);
        }
        let kind = self.find(inner, "TableOrView");
        let view = match self.name(self.first(kind)) {
            "CommentTable" => false,
            "CommentView" => true,
            _ => return self.unsupported(kind),
        };
        let if_exists = self.find(inner, "IfExists") != NONE;
        let mut names = Vec::new();
        for kid in self.kids(inner) {
            if self.name(kid) == "BaseTableName" {
                names.push(self.name_parts(kid));
            }
        }
        let names = self.name_list_slice(names);
        let index = self.ast.drop_tables.len() as u32;
        self.ast.drop_tables.push(DropTable { names, if_exists, view });
        Ok(Statement::DropTable(index))
    }

    /// `InsertStatement <- ... InsertTarget InsertColumnList? InsertValues ...`.
    ///
    /// `ON CONFLICT`, `RETURNING`, `BY NAME`, `BY POSITION`, `OR REPLACE` and the rest of the
    /// clauses the grammar hangs off this are each a refusal, because every one of them changes
    /// what the statement means and none of them changes it in a way anything downstream would
    /// notice if it were dropped.
    fn insert_statement(&mut self, node: u32) -> Result<Statement> {
        for kid in self.kids(node) {
            if matches!(
                self.name(kid),
                "InsertTarget" | "InsertColumnList" | "InsertValues" | "WithClause"
            ) {
                continue;
            }
            return self.unsupported(kid);
        }
        if self.find(node, "WithClause") != NONE {
            return self.unsupported(self.find(node, "WithClause"));
        }
        let name = self.name_parts(self.find(self.find(node, "InsertTarget"), "BaseTableName"));
        let list = self.find(node, "InsertColumnList");
        let columns = if list == NONE {
            Slice::default()
        } else {
            let mut parts = Vec::new();
            for kid in self.kids(self.find(list, "ColumnList")) {
                parts.push(self.identifier(kid));
            }
            self.part_slice(parts)
        };
        let values = self.find(node, "InsertValues");
        let inner = self.first(values);
        if self.name(inner) != "SelectInsertValues" {
            return self.unsupported(inner);
        }
        let source = self.query(self.find(inner, "SelectStatementInternal"))?;
        let index = self.ast.inserts.len() as u32;
        self.ast.inserts.push(Insert { name, columns, source });
        Ok(Statement::Insert(index))
    }

    /// `SelectStatementInternal <- WithClause? SelectSetOpChain ResultModifiers?`.
    fn query(&mut self, node: u32) -> Result<QueryRef> {
        if self.find(node, "WithClause") != NONE {
            return self.unsupported(self.find(node, "WithClause"));
        }
        let chain = self.find(node, "SelectSetOpChain");
        if chain == NONE {
            return self.unsupported(node);
        }
        let query = self.set_op_chain(chain)?;
        let modifiers = self.find(node, "ResultModifiers");
        if modifiers != NONE {
            self.result_modifiers(query, modifiers)?;
        }
        Ok(query)
    }

    /// `SelectSetOpChain <- IntersectChain SelectSetOpChainTail*`, left associative.
    fn set_op_chain(&mut self, node: u32) -> Result<QueryRef> {
        let mut kids = self.kids(node);
        let head = kids.next().unwrap_or(NONE);
        let mut left = self.intersect_chain(head)?;
        for tail in kids {
            // `SelectSetOpChainTail <- SetopClause IntersectChain`.
            let clause = self.first(tail);
            let (op, quantifier, by_name) = self.setop_clause(clause)?;
            let right = self.intersect_chain(self.nth(tail, 1))?;
            left = self.push_query(Query::bare(QueryBody::SetOp {
                op,
                quantifier,
                by_name,
                left,
                right,
            }));
        }
        Ok(left)
    }

    /// `IntersectChain <- SelectAtom IntersectChainTail*`, which binds tighter than union.
    fn intersect_chain(&mut self, node: u32) -> Result<QueryRef> {
        let mut kids = self.kids(node);
        let head = kids.next().unwrap_or(NONE);
        let mut left = self.select_atom(head)?;
        for tail in kids {
            // `IntersectChainTail <- SetIntersectClause SelectAtom`.
            let clause = self.first(tail);
            let quantifier = self.quantifier(self.find(clause, "DistinctOrAll"));
            let right = self.select_atom(self.nth(tail, 1))?;
            left = self.push_query(Query::bare(QueryBody::SetOp {
                op: SetOp::Intersect,
                quantifier,
                by_name: false,
                left,
                right,
            }));
        }
        Ok(left)
    }

    /// `SetopClause <- SetopType DistinctOrAll? ByName?`.
    fn setop_clause(&mut self, node: u32) -> Result<(SetOp, Quantifier, bool)> {
        let kind = self.find(node, "SetopType");
        let op = match self.name(self.first(kind)) {
            "SetopUnion" => SetOp::Union,
            "SetopExcept" => SetOp::Except,
            _ => return self.unsupported(kind),
        };
        let quantifier = self.quantifier(self.find(node, "DistinctOrAll"));
        Ok((op, quantifier, self.find(node, "ByName") != NONE))
    }

    /// `DistinctOrAll <- DistinctKeyword / AllKeyword`, absent included.
    fn quantifier(&self, node: u32) -> Quantifier {
        if node == NONE {
            return Quantifier::Unstated;
        }
        match self.name(self.first(node)) {
            "DistinctKeyword" => Quantifier::Distinct,
            "AllKeyword" => Quantifier::All,
            _ => Quantifier::Unstated,
        }
    }

    /// `SelectAtom <- SelectParens / SelectStatementType`.
    fn select_atom(&mut self, node: u32) -> Result<QueryRef> {
        let inner = self.first(node);
        match self.name(inner) {
            // `SelectParens <- Parens(SelectStatementInternal)`, so the parens buy a query that
            // carries its own order by and limit and nothing else.
            "SelectParens" => self.query(self.first(inner)),
            "SelectStatementType" => {
                let kind = self.first(inner);
                match self.name(kind) {
                    "OptionalParensSimpleSelect" => {
                        let select = self.simple_select(self.unwrap_parens(kind))?;
                        Ok(self.push_query(Query::bare(QueryBody::Select(select))))
                    }
                    "ValuesClause" => {
                        let rows = self.values_clause(kind)?;
                        Ok(self.push_query(Query::bare(QueryBody::Values(rows))))
                    }
                    _ => self.unsupported(kind),
                }
            }
            _ => self.unsupported(inner),
        }
    }

    /// `ValuesClause <- 'VALUES' List(ValuesExpressions)`, each of which is `Parens(List(Expression))`.
    ///
    /// The rows are not checked against each other for width here. Two rows of different widths
    /// parse, and saying so is the binder's job, because the message wants to name the column count
    /// it expected and the parser does not know it for `INSERT` where the table decides.
    fn values_clause(&mut self, node: u32) -> Result<Slice> {
        let mut rows = Vec::new();
        for kid in self.kids(node) {
            if self.name(kid) != "ValuesExpressions" {
                continue;
            }
            let mut items = Vec::new();
            for expr in self.kids(kid) {
                items.push(self.expr(expr)?);
            }
            let slice = self.expr_slice(items);
            rows.push(slice);
        }
        let start = self.ast.rows.len() as u32;
        self.ast.rows.extend(rows);
        Ok(Slice { start, len: self.ast.rows.len() as u32 - start })
    }

    /// `OptionalParensSimpleSelect <- SimpleSelectParens / SimpleSelect`, down to the select.
    fn unwrap_parens(&self, node: u32) -> u32 {
        let mut node = self.first(node);
        while self.name(node) == "SimpleSelectParens" {
            node = self.first(node);
        }
        node
    }

    /// `ResultModifiers <- OrderByClause? LimitOffset?`.
    fn result_modifiers(&mut self, query: QueryRef, node: u32) -> Result<()> {
        let order = self.find(node, "OrderByClause");
        if order != NONE {
            let (items, all) = self.order_by(order)?;
            let start = self.ast.order_items.len() as u32;
            self.ast.order_items.extend(items);
            self.ast.queries[query as usize].order_by =
                Slice { start, len: self.ast.order_items.len() as u32 - start };
            self.ast.queries[query as usize].order_by_all = all;
        }
        let limit = self.find(node, "LimitOffset");
        if limit != NONE {
            self.limit_offset(query, self.first(limit))?;
        }
        Ok(())
    }

    /// The four spellings of a limit and an offset, in either order and either one alone.
    fn limit_offset(&mut self, query: QueryRef, node: u32) -> Result<()> {
        match self.name(node) {
            "LimitOffsetClause" | "OffsetLimitClause" => {
                let limit = self.find(node, "LimitClause");
                if limit != NONE {
                    self.limit(query, limit)?;
                }
                let offset = self.find(node, "OffsetClause");
                if offset != NONE {
                    self.offset(query, offset)?;
                }
                Ok(())
            }
            _ => self.unsupported(node),
        }
    }

    /// `LimitClause <- 'LIMIT' LimitValue`.
    fn limit(&mut self, query: QueryRef, node: u32) -> Result<()> {
        let value = self.first(node);
        let inner = self.first(value);
        match self.name(inner) {
            // `LIMIT ALL` is no limit at all, which is what an absent limit already means.
            "LimitAll" => Ok(()),
            // `LimitExpression <- Expression '%'?`. The percent sign is a terminal so it leaves no
            // node behind, and the only thing that says it was written is the text of the rule that
            // matched it.
            "LimitExpression" => {
                let expr = self.expr(self.first(inner))?;
                self.ast.queries[query as usize].limit = expr;
                self.ast.queries[query as usize].limit_percent = self.text(inner).ends_with('%');
                Ok(())
            }
            "LimitLiteralPercent" => {
                let expr = self.expr(self.first(inner))?;
                self.ast.queries[query as usize].limit = expr;
                self.ast.queries[query as usize].limit_percent = true;
                Ok(())
            }
            _ => self.unsupported(inner),
        }
    }

    /// `OffsetClause <- 'OFFSET' OffsetValue`, where `OffsetValue <- Expression RowOrRows?`.
    fn offset(&mut self, query: QueryRef, node: u32) -> Result<()> {
        let value = self.first(node);
        let expr = self.expr(self.first(value))?;
        self.ast.queries[query as usize].offset = expr;
        Ok(())
    }

    /// `SimpleSelect <- SelectFrom WhereClause? GroupByClause? HavingClause? WindowClause?
    /// QualifyClause? SampleClause?`.
    fn simple_select(&mut self, node: u32) -> Result<SelectRef> {
        for name in ["WindowClause", "QualifyClause", "SampleClause"] {
            let clause = self.find(node, name);
            if clause != NONE {
                return self.unsupported(clause);
            }
        }
        let mut select = Select::empty();
        self.select_from(&mut select, self.first(node))?;
        let filter = self.find(node, "WhereClause");
        if filter != NONE {
            select.filter = self.expr(self.first(filter))?;
        }
        let group = self.find(node, "GroupByClause");
        if group != NONE {
            self.group_by(&mut select, self.first(group))?;
        }
        let having = self.find(node, "HavingClause");
        if having != NONE {
            select.having = self.expr(self.first(having))?;
        }
        Ok(self.push_select(select))
    }

    /// `SelectFrom <- SelectFromClause / FromSelectClause`, which is `SELECT ... FROM ...` and
    /// DuckDB's `FROM ... SELECT ...` written the other way round.
    fn select_from(&mut self, select: &mut Select, node: u32) -> Result<()> {
        let clause = self.first(node);
        let targets = self.find(clause, "SelectClause");
        let from = self.find(clause, "FromClause");
        if from != NONE {
            select.from = self.sources(from)?;
        }
        if targets == NONE {
            // `FROM t` on its own. DuckDB reads it as `SELECT * FROM t`, and inventing the star
            // here rather than in the binder keeps the binder from having to know the shape of the
            // clause that was missing.
            let star = self.push(Expr::Star { qualifier: Slice::default() });
            let start = self.ast.targets.len() as u32;
            self.ast.targets.push(Target { expr: star, alias: NONE });
            select.targets = Slice { start, len: 1 };
            return Ok(());
        }
        self.select_clause(select, targets)
    }

    /// `SelectClause <- 'SELECT' DistinctClause? TargetList?`.
    fn select_clause(&mut self, select: &mut Select, node: u32) -> Result<()> {
        let distinct = self.find(node, "DistinctClause");
        if distinct != NONE {
            let inner = self.first(distinct);
            select.distinct = match self.name(inner) {
                // `SELECT ALL` is the default spelled out.
                "DistinctAll" => Distinct::No,
                "DistinctOn" => {
                    let on = self.find(inner, "DistinctOnTargets");
                    if on == NONE {
                        Distinct::Yes
                    } else {
                        let mut items = Vec::new();
                        for kid in self.kids(on) {
                            items.push(self.expr(kid)?);
                        }
                        Distinct::On(self.expr_slice(items))
                    }
                }
                _ => return self.unsupported(inner),
            };
        }
        let list = self.find(node, "TargetList");
        if list == NONE {
            return Ok(());
        }
        let mut targets = Vec::new();
        for kid in self.kids(list) {
            targets.push(self.target(kid)?);
        }
        let start = self.ast.targets.len() as u32;
        self.ast.targets.extend(targets);
        select.targets = Slice { start, len: self.ast.targets.len() as u32 - start };
        Ok(())
    }

    /// `AliasedExpression <- ColIdExpression / ExpressionAsCollabel / ExpressionOptIdentifier`.
    fn target(&mut self, node: u32) -> Result<Target> {
        let inner = self.first(node);
        match self.name(inner) {
            // `ColIdExpression <- ColId ':' Expression`, the alias written first.
            "ColIdExpression" => {
                let alias = self.identifier(self.first(inner));
                let expr = self.expr(self.nth(inner, 1))?;
                Ok(Target { expr, alias })
            }
            "ExpressionAsCollabel" => {
                let expr = self.expr(self.first(inner))?;
                let alias = self.identifier(self.nth(inner, 1));
                Ok(Target { expr, alias })
            }
            "ExpressionOptIdentifier" => {
                let expr = self.expr(self.first(inner))?;
                let alias =
                    if self.count(inner) > 1 { self.identifier(self.nth(inner, 1)) } else { NONE };
                Ok(Target { expr, alias })
            }
            _ => self.unsupported(inner),
        }
    }

    /// `GroupByClause <- 'GROUP' 'BY' GroupByExpressions`.
    fn group_by(&mut self, select: &mut Select, node: u32) -> Result<()> {
        let inner = self.first(node);
        match self.name(inner) {
            "GroupByAll" => {
                select.group_by_all = true;
                Ok(())
            }
            "GroupByList" => {
                let mut items = Vec::new();
                for kid in self.kids(inner) {
                    // `GroupByExpression <- EmptyGroupingItem / CubeOrRollupClause /
                    // GroupingSetsClause / GroupByBaseExpression`.
                    let expression = self.first(kid);
                    if self.name(expression) != "GroupByBaseExpression" {
                        return self.unsupported(expression);
                    }
                    items.push(self.expr(self.first(expression))?);
                }
                select.group_by = self.expr_slice(items);
                Ok(())
            }
            _ => self.unsupported(inner),
        }
    }

    /// `OrderByClause <- 'ORDER' 'BY' OrderByExpressions`, where `OrderByExpressions <- OrderByAll
    /// / OrderByExpressionList`.
    fn order_by(&mut self, node: u32) -> Result<(Vec<OrderItem>, bool)> {
        let inner = self.first(self.first(node));
        match self.name(inner) {
            "OrderByAll" => {
                let (order, nulls) = self.sort_options(inner);
                Ok((vec![OrderItem { expr: NONE, order, nulls }], true))
            }
            "OrderByExpressionList" => {
                let mut items = Vec::new();
                for kid in self.kids(inner) {
                    // `OrderByExpression <- Expression DescOrAsc? NullsFirstOrLast?`.
                    let expr = self.expr(self.first(kid))?;
                    let (order, nulls) = self.sort_options(kid);
                    items.push(OrderItem { expr, order, nulls });
                }
                Ok((items, false))
            }
            _ => self.unsupported(inner),
        }
    }

    /// The direction and the null placement of one sort key, either of which may be unwritten.
    fn sort_options(&self, node: u32) -> (Order, Nulls) {
        let direction = self.find(node, "DescOrAsc");
        let order = if direction == NONE {
            Order::Unstated
        } else if self.name(self.first(direction)) == "DescendingOrder" {
            Order::Descending
        } else {
            Order::Ascending
        };
        let placement = self.find(node, "NullsFirstOrLast");
        let nulls = if placement == NONE {
            Nulls::Unstated
        } else if self.name(self.first(placement)) == "NullsFirst" {
            Nulls::First
        } else {
            Nulls::Last
        };
        (order, nulls)
    }

    // From clauses.

    /// `FromClause <- 'FROM' List(TableRef)`.
    fn sources(&mut self, node: u32) -> Result<Slice> {
        let mut items = Vec::new();
        for kid in self.kids(node) {
            items.push(self.table_ref(kid)?);
        }
        let start = self.ast.source_lists.len() as u32;
        self.ast.source_lists.extend(items);
        Ok(Slice { start, len: self.ast.source_lists.len() as u32 - start })
    }

    /// `TableRef <- InnerTableRef JoinOrPivot*`, left associative like the set operators.
    fn table_ref(&mut self, node: u32) -> Result<SourceRef> {
        let mut kids = self.kids(node);
        let head = kids.next().unwrap_or(NONE);
        let mut left = self.inner_table_ref(head)?;
        for tail in kids {
            let clause = self.first(tail);
            if self.name(clause) != "JoinClause" {
                return self.unsupported(clause);
            }
            left = self.join(left, self.first(clause))?;
        }
        Ok(left)
    }

    /// `InnerTableRef <- ValuesRef / TableFunction / TableSubquery / BaseTableRef / ParensTableRef`.
    fn inner_table_ref(&mut self, node: u32) -> Result<SourceRef> {
        let inner = if self.name(node) == "InnerTableRef" { self.first(node) } else { node };
        match self.name(inner) {
            "BaseTableRef" => {
                if self.find(inner, "TableAliasColon") != NONE {
                    return self.unsupported(inner);
                }
                for name in ["AtClause", "SampleClause"] {
                    let clause = self.find(inner, name);
                    if clause != NONE {
                        return self.unsupported(clause);
                    }
                }
                let name = self.name_parts(self.find(inner, "BaseTableName"));
                let (alias, columns) = self.table_alias(self.find(inner, "TableAlias"));
                Ok(self.push_source(Source::Table { name, alias, columns }))
            }
            "TableSubquery" => {
                if self.find(inner, "TableAliasColon") != NONE
                    || self.find(inner, "Lateral") != NONE
                {
                    return self.unsupported(inner);
                }
                // `SubqueryReference <- Parens(SelectStatementInternal)`.
                let reference = self.find(inner, "SubqueryReference");
                let query = self.query(self.first(reference))?;
                let (alias, columns) = self.table_alias(self.find(inner, "TableAlias"));
                Ok(self.push_source(Source::Subquery { query, alias, columns }))
            }
            // `TableFunction <- TableFunctionLateralOpt / TableFunctionAliasColon`, and
            // `TableFunctionLateralOpt <- Lateral? QualifiedTableFunction TableFunctionArguments
            // WithOrdinality? TableAlias?`. The colon form and `LATERAL` are their own work, and
            // `WITH ORDINALITY` adds a column, so all three are turned away rather than dropped.
            "TableFunction" => {
                let form = self.first(inner);
                for name in ["TableAliasColon", "Lateral", "WithOrdinality", "SampleClause"] {
                    let clause = self.find(form, name);
                    if clause != NONE {
                        return self.unsupported(clause);
                    }
                }
                let name = self.name_parts(self.find(form, "QualifiedTableFunction"));
                let mut args = Vec::new();
                // `TableFunctionArguments <- Parens(List(FunctionArgument)?)`, so a call with no
                // arguments has the wrapper and no list under it.
                let list = self.find(form, "TableFunctionArguments");
                for kid in self.kids(list) {
                    args.push(self.argument(kid)?);
                }
                let args = self.expr_slice(args);
                let (alias, columns) = self.table_alias(self.find(form, "TableAlias"));
                Ok(self.push_source(Source::Function { name, args, alias, columns }))
            }
            "ValuesRef" => {
                if self.find(inner, "TableAliasColon") != NONE {
                    return self.unsupported(inner);
                }
                let rows = self.values_clause(self.find(inner, "ValuesClause"))?;
                let (alias, columns) = self.table_alias(self.find(inner, "TableAlias"));
                Ok(self.push_source(Source::Values { rows, alias, columns }))
            }
            "ParensTableRef" => {
                if self.find(inner, "TableAliasColon") != NONE
                    || self.find(inner, "SampleClause") != NONE
                    || self.find(inner, "TableAlias") != NONE
                {
                    return self.unsupported(inner);
                }
                self.table_ref(self.find(inner, "TableRef"))
            }
            _ => self.unsupported(inner),
        }
    }

    /// `TableAlias <- TableAliasAs / TableAliasWithoutAs`, either with a column alias list.
    fn table_alias(&mut self, node: u32) -> (StrRef, Slice) {
        if node == NONE {
            return (NONE, Slice::default());
        }
        let inner = self.first(node);
        let alias = self.identifier(self.first(inner));
        let list = self.find(inner, "ColumnAliases");
        if list == NONE {
            return (alias, Slice::default());
        }
        let mut columns = Vec::new();
        for kid in self.kids(list) {
            let name = self.identifier(kid);
            columns.push(name);
        }
        (alias, self.part_slice(columns))
    }

    /// `JoinClause <- JoinByClause / RegularJoinClause / JoinWithoutOnClause / NearestJoinClause`.
    fn join(&mut self, left: SourceRef, node: u32) -> Result<SourceRef> {
        match self.name(node) {
            // `RegularJoinClause <- Asof? JoinType? 'JOIN' TableRef JoinQualifier`.
            "RegularJoinClause" => {
                if self.find(node, "Asof") != NONE {
                    return self.unsupported(node);
                }
                let kind = self.join_type(self.find(node, "JoinType"));
                let right = self.table_ref(self.find(node, "TableRef"))?;
                let (on, using) = self.join_qualifier(self.find(node, "JoinQualifier"))?;
                Ok(self.push_source(Source::Join { left, right, kind, natural: false, on, using }))
            }
            // `JoinWithoutOnClause <- JoinPrefix 'JOIN' InnerTableRef`, which is cross, natural and
            // positional. Those three are exactly the joins that carry no condition.
            "JoinWithoutOnClause" => {
                let prefix = self.first(self.find(node, "JoinPrefix"));
                let (kind, natural) = match self.name(prefix) {
                    "CrossJoinPrefix" => (JoinKind::Cross, false),
                    "PositionalJoinPrefix" => (JoinKind::Positional, false),
                    "NaturalJoinPrefix" => (self.join_type(self.find(prefix, "JoinType")), true),
                    _ => return self.unsupported(prefix),
                };
                let right = self.inner_table_ref(self.find(node, "InnerTableRef"))?;
                Ok(self.push_source(Source::Join {
                    left,
                    right,
                    kind,
                    natural,
                    on: NONE,
                    using: Slice::default(),
                }))
            }
            _ => self.unsupported(node),
        }
    }

    /// `JoinType <- FullJoin / LeftJoin / RightJoin / SemiJoin / AntiJoin / InnerJoin`, absent
    /// meaning inner, which is what SQL has always meant by a bare `JOIN`.
    fn join_type(&self, node: u32) -> JoinKind {
        if node == NONE {
            return JoinKind::Inner;
        }
        match self.name(self.first(node)) {
            "FullJoin" => JoinKind::Full,
            "LeftJoin" => JoinKind::Left,
            "RightJoin" => JoinKind::Right,
            "SemiJoin" => JoinKind::Semi,
            "AntiJoin" => JoinKind::Anti,
            _ => JoinKind::Inner,
        }
    }

    /// `JoinQualifier <- OnClause / UsingClause`.
    fn join_qualifier(&mut self, node: u32) -> Result<(ExprRef, Slice)> {
        let inner = self.first(node);
        match self.name(inner) {
            "OnClause" => Ok((self.expr(self.first(inner))?, Slice::default())),
            "UsingClause" => {
                let mut columns = Vec::new();
                for kid in self.kids(inner) {
                    let name = self.identifier(kid);
                    columns.push(name);
                }
                Ok((NONE, self.part_slice(columns)))
            }
            _ => self.unsupported(inner),
        }
    }

    // Expressions.

    /// One expression, from wherever in the precedence chain it starts.
    ///
    /// The loop is the whole design. A rule that says something gets an arm, a rule with exactly
    /// one child said nothing and is stepped through, and anything else is an error naming itself.
    /// The chain rules never get an arm for their one child case, which is why adding a precedence
    /// level upstream costs nothing here.
    fn expr(&mut self, node: u32) -> Result<ExprRef> {
        let mut node = node;
        loop {
            let count = self.count(node);
            let name = self.name(node);
            match name {
                "LogicalOrExpression" if count > 1 => return self.logical(node, BinaryOp::Or),
                "LogicalAndExpression" if count > 1 => return self.logical(node, BinaryOp::And),
                "LogicalNotExpression" if count > 1 => return self.logical_not(node),
                "IsExpression" if count > 1 => return self.is_expression(node),
                "BetweenInLikeExpression" if count > 1 => return self.between_in_like(node),
                "PrefixExpression" if count > 1 => return self.prefix(node),
                "BaseExpression" if count > 1 => return self.indirection(node),
                "LambdaArrowExpression"
                | "IsDistinctFromExpression"
                | "ComparisonExpression"
                | "OtherOperatorExpression"
                | "BitwiseExpression"
                | "AdditiveExpression"
                | "MultiplicativeExpression"
                | "ExponentiationExpression"
                | "CollateExpression"
                | "AtTimeZoneExpression"
                    if count > 1 =>
                {
                    return self.tail_chain(node);
                }
                "ColumnReference" => {
                    let name = self.name_parts(node);
                    return Ok(self.push(Expr::Column { name }));
                }
                "StarExpression" => return self.star(node),
                "NumberLiteral" => {
                    let text = self.text(node).to_string();
                    let text = self.intern(&text);
                    return Ok(self.push(Expr::Literal { kind: LiteralKind::Number, text }));
                }
                "StringLiteral" => {
                    let text = self.string_value(node);
                    let text = self.intern(&text);
                    return Ok(self.push(Expr::Literal { kind: LiteralKind::String, text }));
                }
                "NullLiteral" | "TrueLiteral" | "FalseLiteral" => {
                    let kind = match name {
                        "NullLiteral" => LiteralKind::Null,
                        "TrueLiteral" => LiteralKind::True,
                        _ => LiteralKind::False,
                    };
                    return Ok(self.push(Expr::Literal { kind, text: NONE }));
                }
                "FunctionExpression" => return self.function(node),
                "ExtractExpression" => return self.extract(node),
                "CastExpression" => return self.cast(node),
                "CaseExpression" => return self.case(node),
                "ParenthesisExpression" => return self.row(node),
                "BoundedListExpression" => return self.list(node),
                "QuestionMarkNumberedParameter"
                | "AnonymousParameter"
                | "NumberedParameter"
                | "ColLabelParameter" => return self.parameter(node),
                "SubqueryExpression" => return self.subquery(node),
                _ if count == 1 => node = self.first(node),
                _ => return self.unsupported(node),
            }
        }
    }

    /// `X <- Y XTail*` where `XTail <- Operator Y`, the shape ten precedence levels share.
    fn tail_chain(&mut self, node: u32) -> Result<ExprRef> {
        let mut kids = self.kids(node);
        let head = kids.next().unwrap_or(NONE);
        let mut left = self.expr(head)?;
        for tail in kids {
            let operator = self.first(tail);
            let op = self.binary_op(operator)?;
            // `ComparisonExpressionTail <- ComparisonOperator NotExpression? BetweenInLikeExpression`
            // is the one tail with an optional middle, so the operand is the last child and not the
            // second one. Taking the last is right for every tail and wrong for none.
            let operand = self.kids(tail).last().unwrap_or(NONE);
            if self.count(tail) > 2 {
                return self.unsupported(tail);
            }
            let right = self.expr(operand)?;
            left = self.push(Expr::Binary { op, left, right });
        }
        Ok(left)
    }

    /// Which infix operator a tail's operator node is.
    fn binary_op(&mut self, node: u32) -> Result<BinaryOp> {
        // The operator rules nest: `ComparisonOperator` over `OperatorGreaterThan` over the symbol
        // itself. Every one of them covers the same tokens, so the text is the same at every level
        // and reading it once at the top is enough. The name is not, which is why the bottom of the
        // chain is walked to as well: `OtherOperator` says nothing and `OperatorLiteral` says
        // everything, and they are three levels apart.
        let mut leaf = node;
        while self.count(leaf) == 1 {
            leaf = self.first(leaf);
        }
        let text = self.text(node);
        let upper = text.to_ascii_uppercase();
        let op = match upper.as_str() {
            "OR" => BinaryOp::Or,
            "AND" => BinaryOp::And,
            "=" | "==" => BinaryOp::Eq,
            "!=" | "<>" => BinaryOp::NotEq,
            "<" => BinaryOp::Lt,
            ">" => BinaryOp::Gt,
            "<=" => BinaryOp::LtEq,
            ">=" => BinaryOp::GtEq,
            "+" => BinaryOp::Add,
            "-" => BinaryOp::Subtract,
            "*" => BinaryOp::Multiply,
            "/" => BinaryOp::Divide,
            "//" => BinaryOp::IntegerDivide,
            "%" => BinaryOp::Modulo,
            "^" | "**" => BinaryOp::Power,
            "&" => BinaryOp::BitAnd,
            "|" => BinaryOp::BitOr,
            "<<" => BinaryOp::ShiftLeft,
            ">>" => BinaryOp::ShiftRight,
            "||" => BinaryOp::Concat,
            "COLLATE" => BinaryOp::Collate,
            "->" => BinaryOp::Arrow,
            "->>" => BinaryOp::LongArrow,
            "@>" => BinaryOp::Contains,
            "<@" => BinaryOp::ContainedBy,
            "&&" => BinaryOp::Overlaps,
            "^@" => BinaryOp::StartsWith,
            "<<=" => BinaryOp::InetContainedByOrEq,
            ">>=" => BinaryOp::InetContainsOrEq,
            _ if self.name(leaf) == "AtTimeZoneOperator" => BinaryOp::AtTimeZone,
            // `IsDistinctFromOp <- 'IS' 'NOT'? 'DISTINCT' 'FROM'`, told apart by the middle word,
            // which is not in the tree because keywords are terminals.
            _ if self.name(leaf) == "IsDistinctFromOp" => {
                if upper.split_whitespace().any(|word| word == "NOT") {
                    BinaryOp::IsNotDistinctFrom
                } else {
                    BinaryOp::IsDistinctFrom
                }
            }
            // `OperatorLiteral` is the open end of the operator set. Its body in the grammar text
            // says `Identifier`, but it is one of the 24 rules whose body the matcher does not
            // walk and the matcher it is overridden to is the bare operator one, so what it
            // actually accepts is any run of operator characters that is not already a token.
            // `a <=> b` is such a run, DuckDB resolves it as a two argument function of that name,
            // and rejecting it here would reject SQL DuckDB accepts.
            _ if self.name(leaf) == "OperatorLiteral" => {
                let interned = self.intern(text);
                BinaryOp::Named(interned)
            }
            _ => return self.unsupported(node),
        };
        Ok(op)
    }

    /// `LogicalOrExpression <- LogicalAndExpression LogicalOrExpressionTail*`, and the `AND` twin.
    ///
    /// Separate from the other tails because the tail here is `'OR' LogicalAndExpression` with the
    /// keyword as a terminal, so there is no operator node to read and the operator is the rule.
    fn logical(&mut self, node: u32, op: BinaryOp) -> Result<ExprRef> {
        let mut kids = self.kids(node);
        let head = kids.next().unwrap_or(NONE);
        let mut left = self.expr(head)?;
        for tail in kids {
            let right = self.expr(self.first(tail))?;
            left = self.push(Expr::Binary { op, left, right });
        }
        Ok(left)
    }

    /// `LogicalNotExpression <- NotExpression? IsExpression`, where `NotExpression <- NotKeyword+`.
    ///
    /// The plus matters. `NOT NOT x` is two nodes in the parse tree and two negations in the AST,
    /// and folding them here would be an optimizer decision taken in the parser.
    fn logical_not(&mut self, node: u32) -> Result<ExprRef> {
        let negations = self.count(self.first(node));
        let mut expr = self.expr(self.nth(node, 1))?;
        for _ in 0..negations {
            expr = self.push(Expr::Unary { op: UnaryOp::Not, operand: expr });
        }
        Ok(expr)
    }

    /// `IsExpression <- IsDistinctFromExpression IsTest*`, the postfix null and boolean tests.
    fn is_expression(&mut self, node: u32) -> Result<ExprRef> {
        let mut kids = self.kids(node);
        let head = kids.next().unwrap_or(NONE);
        let mut expr = self.expr(head)?;
        for test in kids {
            let inner = self.first(test);
            let negated = self.text(inner).to_ascii_uppercase().contains("NOT");
            let op = match self.name(inner) {
                "NotNull" => UnaryOp::IsNotNull,
                "IsNull" => UnaryOp::IsNull,
                // `IsLiteral <- 'IS' 'NOT'? IsLiteralValue`, and the value rule is one more level
                // down again because it is a choice of four and not four alternatives inlined.
                "IsLiteral" => match self.name(self.first(self.first(inner))) {
                    "NullLiteral" if negated => UnaryOp::IsNotNull,
                    "NullLiteral" => UnaryOp::IsNull,
                    "TrueLiteral" if negated => UnaryOp::IsNotTrue,
                    "TrueLiteral" => UnaryOp::IsTrue,
                    "FalseLiteral" if negated => UnaryOp::IsNotFalse,
                    "FalseLiteral" => UnaryOp::IsFalse,
                    "UnknownLiteral" if negated => UnaryOp::IsNotUnknown,
                    "UnknownLiteral" => UnaryOp::IsUnknown,
                    _ => return self.unsupported(inner),
                },
                _ => return self.unsupported(inner),
            };
            expr = self.push(Expr::Unary { op, operand: expr });
        }
        Ok(expr)
    }

    /// `BetweenInLikeExpression <- OtherOperatorExpression BetweenInLikeOp?`.
    fn between_in_like(&mut self, node: u32) -> Result<ExprRef> {
        let operand = self.expr(self.first(node))?;
        // `BetweenInLikeOp <- 'NOT'? BetweenInLikeOpExpression`. The `NOT` is a terminal, so what
        // says it was written is that the op node covers a token the inner node does not.
        let op = self.nth(node, 1);
        let negated = self.text(op).to_ascii_uppercase().starts_with("NOT");
        let inner = self.first(self.first(op));
        match self.name(inner) {
            // `BetweenClause <- 'BETWEEN' x 'AND' y`.
            "BetweenClause" => {
                let low = self.expr(self.first(inner))?;
                let high = self.expr(self.nth(inner, 1))?;
                Ok(self.push(Expr::Between { operand, low, high, negated }))
            }
            // `InClause <- 'IN' InExpression`.
            "InClause" => {
                let expression = self.first(self.first(inner));
                match self.name(expression) {
                    "InExpressionList" => {
                        let mut items = Vec::new();
                        for kid in self.kids(expression) {
                            items.push(self.expr(kid)?);
                        }
                        let list = self.expr_slice(items);
                        Ok(self.push(Expr::In { operand, list, negated }))
                    }
                    _ => self.unsupported(expression),
                }
            }
            // `LikeClause <- LikeVariations x EscapeClause?`.
            "LikeClause" => {
                if self.find(inner, "EscapeClause") != NONE {
                    return self.unsupported(inner);
                }
                let variation = self.name(self.first(self.first(inner)));
                let op = match (variation, negated) {
                    ("LikeToken", false) | ("NotLikeOp", true) => BinaryOp::Like,
                    ("LikeToken", true) | ("NotLikeOp", false) => BinaryOp::NotLike,
                    ("ILikeToken", false) | ("NotILikeOp", true) => BinaryOp::ILike,
                    ("ILikeToken", true) | ("NotILikeOp", false) => BinaryOp::NotILike,
                    // Glob and the bare regex match have no negated spelling of their own in
                    // `LikeVariations`, so a `NOT` in front of either stays an explicit negation.
                    ("GlobToken", _) => BinaryOp::Glob,
                    ("RegexMatchToken", _) => BinaryOp::Regex,
                    ("SimilarToToken", false) | ("NotSimilarToOp", true) => BinaryOp::SimilarTo,
                    ("SimilarToToken", true) | ("NotSimilarToOp", false) => BinaryOp::NotSimilarTo,
                    ("RegexInsensitiveMatchToken", false)
                    | ("NotRegexInsensitiveMatchOp", true) => BinaryOp::RegexInsensitive,
                    ("RegexInsensitiveMatchToken", true)
                    | ("NotRegexInsensitiveMatchOp", false) => BinaryOp::NotRegexInsensitive,
                    _ => return self.unsupported(inner),
                };
                let right = self.expr(self.nth(inner, 1))?;
                let expr = self.push(Expr::Binary { op, left: operand, right });
                // The like family folds its negation into the operator because it has a spelling
                // for the negated form. Glob and regex do not, so theirs stays where it was.
                if negated && matches!(op, BinaryOp::Glob | BinaryOp::Regex) {
                    return Ok(self.push(Expr::Unary { op: UnaryOp::Not, operand: expr }));
                }
                Ok(expr)
            }
            _ => self.unsupported(inner),
        }
    }

    /// `PrefixExpression <- PrefixOperator* BaseExpression`, applied right to left.
    fn prefix(&mut self, node: u32) -> Result<ExprRef> {
        let kids: Vec<u32> = self.kids(node).collect();
        let mut expr = self.expr(kids[kids.len() - 1])?;
        for &operator in kids[..kids.len() - 1].iter().rev() {
            let op = match self.name(self.first(operator)) {
                "MinusPrefixOperator" => UnaryOp::Negate,
                "PlusPrefixOperator" => UnaryOp::Plus,
                "TildePrefixOperator" => UnaryOp::BitNot,
                _ => return self.unsupported(operator),
            };
            expr = self.push(Expr::Unary { op, operand: expr });
        }
        Ok(expr)
    }

    /// `BaseExpression <- SingleExpression IndirectionList?`, the postfix chain.
    fn indirection(&mut self, node: u32) -> Result<ExprRef> {
        let mut expr = self.expr(self.first(node))?;
        for step in self.kids(self.nth(node, 1)) {
            let inner = self.first(step);
            expr = match self.name(inner) {
                // `CastOperator <- '::' Type`.
                "CastOperator" => {
                    let text = self.text(self.first(inner)).to_string();
                    let ty = self.intern(&text);
                    self.push(Expr::Cast { operand: expr, ty, try_cast: false })
                }
                "DotOperator" => {
                    let dot = self.first(inner);
                    match self.name(dot) {
                        // `DotColumnOperator <- '.' ColLabel`, which DuckDB resolves as a call of
                        // `struct_extract`. Writing it as that call rather than as its own node
                        // keeps the binder from needing a rule for a thing that is already a
                        // function.
                        "DotColumnOperator" => {
                            let field = self.identifier(self.first(dot));
                            let text = self.ast.string(field).to_string();
                            let literal = self.intern(&text);
                            let key = self
                                .push(Expr::Literal { kind: LiteralKind::String, text: literal });
                            let name = self.function_name("struct_extract");
                            let args = self.expr_slice(vec![expr, key]);
                            self.push(Expr::Function { name, args, distinct: false })
                        }
                        // `DotMethodOperator <- '.' MethodExpression`, where `x.f(a)` is `f(x, a)`.
                        "DotMethodOperator" => {
                            let method = self.first(dot);
                            let text = self.text(self.first(method)).to_string();
                            let text = unquote(&text);
                            let name = self.function_name(&text);
                            let mut args = vec![expr];
                            let list = self.find(method, "MethodExpressionArguments");
                            if list != NONE {
                                let inner = self.first(list);
                                let arguments = self.find(inner, "MethodFunctionArguments");
                                if arguments != NONE {
                                    for kid in self.kids(arguments) {
                                        args.push(self.argument(kid)?);
                                    }
                                }
                            }
                            let args = self.expr_slice(args);
                            self.push(Expr::Function { name, args, distinct: false })
                        }
                        _ => return self.unsupported(dot),
                    }
                }
                // `SliceExpression <- '[' SliceBound ']'`, one index or a range.
                "SliceExpression" => {
                    let bound = self.first(inner);
                    let has_end = self.find(bound, "EndSliceBound") != NONE;
                    let has_step = self.find(bound, "StepSliceBound") != NONE;
                    if has_end || has_step {
                        return self.unsupported(inner);
                    }
                    let index = self.expr(self.first(bound))?;
                    let name = self.function_name("array_extract");
                    let args = self.expr_slice(vec![expr, index]);
                    self.push(Expr::Function { name, args, distinct: false })
                }
                // `PostfixOperator <- '!'`.
                "PostfixOperator" => {
                    self.push(Expr::Unary { op: UnaryOp::Factorial, operand: expr })
                }
                _ => return self.unsupported(inner),
            };
        }
        Ok(expr)
    }

    /// A one part function name, for the calls the transformer invents rather than reads.
    fn function_name(&mut self, name: &str) -> Slice {
        let interned = self.intern(name);
        self.part_slice(vec![interned])
    }

    /// `StarExpression <- StarQualifierList? '*' ExcludeList? ReplaceList? RenameList?`.
    fn star(&mut self, node: u32) -> Result<ExprRef> {
        for name in ["ExcludeList", "ReplaceList", "RenameList"] {
            let list = self.find(node, name);
            if list != NONE {
                return self.unsupported(list);
            }
        }
        let qualifier = self.find(node, "StarQualifierList");
        let qualifier =
            if qualifier == NONE { Slice::default() } else { self.name_parts(qualifier) };
        Ok(self.push(Expr::Star { qualifier }))
    }

    /// `FunctionExpression <- FunctionIdentifier FunctionExpressionArguments WithinGroupClause?
    /// FilterClause? ExportClause? OverClause?`.
    fn function(&mut self, node: u32) -> Result<ExprRef> {
        for name in ["WithinGroupClause", "FilterClause", "ExportClause", "OverClause"] {
            let clause = self.find(node, name);
            if clause != NONE {
                return self.unsupported(clause);
            }
        }
        let name = self.name_parts(self.first(node));
        // `FunctionExpressionArguments <- Parens(FunctionExpressionArgumentList)` and
        // `FunctionExpressionArgumentList <- DistinctOrAll? FunctionArgumentList? OrderByClause?
        // IgnoreOrRespectNulls?`, so a call with no arguments still has both wrappers.
        let list = self.first(self.nth(node, 1));
        for name in ["OrderByClause", "IgnoreOrRespectNulls"] {
            let clause = self.find(list, name);
            if clause != NONE {
                return self.unsupported(clause);
            }
        }
        let distinct = self.quantifier(self.find(list, "DistinctOrAll")) == Quantifier::Distinct;
        let mut args = Vec::new();
        let arguments = self.find(list, "FunctionArgumentList");
        if arguments != NONE {
            for kid in self.kids(arguments) {
                args.push(self.argument(kid)?);
            }
        }
        let args = self.expr_slice(args);
        Ok(self.push(Expr::Function { name, args, distinct }))
    }

    /// `ExtractExpression <- 'EXTRACT' Parens(ExtractArguments)` and
    /// `ExtractArguments <- ExtractArgument 'FROM' Expression`.
    ///
    /// `EXTRACT` is not a function in the grammar because its argument list is not an argument
    /// list, and it is a function everywhere after here because DuckDB's parser does the same
    /// rewrite: `EXTRACT(minute FROM t)` is `date_part('minute', t)` and there is no separate
    /// implementation of one of them. The part is a keyword, an identifier or a string in the
    /// grammar, and all three become the string, which is why this is a rewrite and not a node.
    fn extract(&mut self, node: u32) -> Result<ExprRef> {
        let arguments = self.find(node, "ExtractArguments");
        if arguments == NONE {
            return self.unsupported(node);
        }
        let argument = self.first(self.first(arguments));
        let part = match self.name(argument) {
            "ExtractStringArgument" => self.string_value(argument),
            // A keyword or an identifier, both taken as written. Which specifier names are legal is
            // not a question about syntax, so the answer to it lives with the function.
            "ExtractDatePartArgument" | "ExtractIdentifierArgument" => {
                self.text(argument).to_string()
            }
            _ => return self.unsupported(argument),
        };
        let text = self.intern(&part);
        let part = self.push(Expr::Literal { kind: LiteralKind::String, text });
        let operand = self.expr(self.nth(arguments, 1))?;
        let name = self.function_name("date_part");
        let args = self.expr_slice(vec![part, operand]);
        Ok(self.push(Expr::Function { name, args, distinct: false }))
    }

    /// `FunctionArgument <- NamedFunctionArgument / PositionalFunctionArgument`.
    fn argument(&mut self, node: u32) -> Result<ExprRef> {
        let inner = self.first(node);
        match self.name(inner) {
            "PositionalFunctionArgument" => self.expr(self.first(inner)),
            _ => self.unsupported(inner),
        }
    }

    /// `CastExpression <- CastOrTryCast Parens(CastArguments)`.
    fn cast(&mut self, node: u32) -> Result<ExprRef> {
        let try_cast = self.name(self.first(self.first(node))) == "TryCastKeyword";
        // `CastArguments <- Expression 'AS' Type`.
        let arguments = self.nth(node, 1);
        let operand = self.expr(self.first(arguments))?;
        let text = self.text(self.nth(arguments, 1)).to_string();
        let ty = self.intern(&text);
        Ok(self.push(Expr::Cast { operand, ty, try_cast }))
    }

    /// `CaseExpression <- 'CASE' Expression? CaseWhenThen+ CaseElse? 'END'`.
    fn case(&mut self, node: u32) -> Result<ExprRef> {
        let mut operand = NONE;
        let mut arms = Vec::new();
        let mut otherwise = NONE;
        for kid in self.kids(node) {
            match self.name(kid) {
                // `CaseWhenThen <- 'WHEN' Expression 'THEN' Expression`.
                "CaseWhenThen" => {
                    let when = self.expr(self.first(kid))?;
                    let then = self.expr(self.nth(kid, 1))?;
                    arms.push(CaseArm { when, then });
                }
                // `CaseElse <- 'ELSE' Expression`.
                "CaseElse" => otherwise = self.expr(self.first(kid))?,
                // The bare `Expression` before the first `WHEN`, which makes it a simple case.
                _ => operand = self.expr(kid)?,
            }
        }
        let start = self.ast.case_arms.len() as u32;
        self.ast.case_arms.extend(arms);
        let arms = Slice { start, len: self.ast.case_arms.len() as u32 - start };
        Ok(self.push(Expr::Case { operand, arms, otherwise }))
    }

    /// `ParenthesisExpression <- Parens(List(Expression)?)`, which is a row value.
    ///
    /// One item is not a row. `(a)` is `a` in every dialect and reading it as a one column row
    /// would change what `(a) = (b)` means.
    fn row(&mut self, node: u32) -> Result<ExprRef> {
        let mut items = Vec::new();
        for kid in self.kids(node) {
            items.push(self.expr(kid)?);
        }
        if items.len() == 1 {
            return Ok(items[0]);
        }
        let items = self.expr_slice(items);
        Ok(self.push(Expr::Row { items }))
    }

    /// `Parameter <- '?' Number / '?' / '$' Number / '$' ColLabel`, a prepared statement parameter.
    ///
    /// The identifier is what follows the marker, so `?1` and `$1` are both the parameter named 1,
    /// and a bare `?` takes the next number by where it was written. That is what DuckDB does, which
    /// is why `? + $2` prints as `$1 + $2`: the counting is its own and does not skip a number
    /// because a later parameter claimed it.
    fn parameter(&mut self, node: u32) -> Result<ExprRef> {
        let written = self.text(node).trim();
        let written = written.trim_start_matches(['?', '$']).trim();
        let name = if written.is_empty() {
            self.anonymous += 1;
            self.anonymous.to_string()
        } else {
            written.to_string()
        };
        let name = self.intern(&name);
        Ok(self.push(Expr::Parameter { name }))
    }

    /// `BoundedListExpression <- '[' List(Expression)? ']'`, which is a LIST value.
    ///
    /// One item is a list of one here, unlike the parenthesised form, because the brackets are what
    /// say list and there is nothing else `[a]` could mean.
    fn list(&mut self, node: u32) -> Result<ExprRef> {
        let mut items = Vec::new();
        for kid in self.kids(node) {
            items.push(self.expr(kid)?);
        }
        let items = self.expr_slice(items);
        Ok(self.push(Expr::List { items }))
    }

    /// `SubqueryExpression <- SubqueryNot? SubqueryExists? SubqueryReference`.
    fn subquery(&mut self, node: u32) -> Result<ExprRef> {
        if self.find(node, "SubqueryNot") != NONE || self.find(node, "SubqueryExists") != NONE {
            return self.unsupported(node);
        }
        let reference = self.find(node, "SubqueryReference");
        let query = self.query(self.first(reference))?;
        Ok(self.push(Expr::Subquery { query }))
    }

    /// The value of a string literal, with the quotes gone and the escapes resolved.
    ///
    /// A literal can be several tokens. `'a' 'b'` on two lines is one literal that is `ab`, which is
    /// the SQL standard's rule and DuckDB's, so the node is decoded token by token rather than by
    /// taking its text and stripping the outside.
    fn string_value(&self, node: u32) -> String {
        let span = self.tree.node(node);
        let mut value = String::new();
        for token in &self.tokens[span.start as usize..span.end as usize] {
            if token.kind != Kind::String {
                continue;
            }
            let text = token.text(self.query);
            match text.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')) {
                Some(body) => value.push_str(&body.replace("''", "'")),
                None => value.push_str(text),
            }
        }
        value
    }
}

/// Strip the quoting off an identifier.
///
/// DuckDB does not fold identifier case at any point, quoted or not, so this only removes the
/// quotes and resolves the doubled ones. Anything else would be the parser deciding what a name is.
///
/// Single quotes are stripped too, and the only way one gets here is the file name in `FROM
/// 'hits.parquet'`, because the matcher takes a string for a name in that position and in `COPY t TO
/// '...'` and nowhere else. Leaving them on would make that name different from the one `FROM
/// "hits.parquet"` writes, and DuckDB reads both of those as the same file.
fn unquote(text: &str) -> String {
    if let Some(body) = text.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
        return body.replace("\"\"", "\"");
    }
    match text.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')) {
        Some(body) => body.replace("''", "'"),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CORPUS;
    use crate::matcher::parse;

    /// The AST written back out as text, which is what the assertions below read.
    ///
    /// Not a SQL printer and not trying to be. It is deliberately not valid SQL: operators are
    /// spelled with the name of the variant and every binary node is parenthesised, so that a test
    /// asserting on this text is asserting on the shape of the tree and not on a formatting choice.
    /// `a - b - c` and `a - (b - c)` have to look different here or the test that tells them apart
    /// is not a test.
    fn show(ast: &Ast, expr: ExprRef) -> String {
        if expr == NONE {
            return "-".to_string();
        }
        let list = |slice: Slice| {
            ast.expr_list(slice).iter().map(|&item| show(ast, item)).collect::<Vec<_>>().join(", ")
        };
        match ast.expr(expr) {
            Expr::Star { qualifier } if qualifier.is_empty() => "*".to_string(),
            Expr::Star { qualifier } => format!("{}.*", ast.name_text(qualifier)),
            Expr::Column { name } => ast.name_text(name),
            Expr::Literal { kind, text } => match kind {
                LiteralKind::Number => ast.string(text).to_string(),
                LiteralKind::String => format!("'{}'", ast.string(text)),
                other => format!("{other:?}").to_uppercase(),
            },
            Expr::Unary { op, operand } => format!("({op:?} {})", show(ast, operand)),
            Expr::Binary { op, left, right } => {
                let op = match op {
                    BinaryOp::Named(name) => ast.string(name).to_string(),
                    other => format!("{other:?}"),
                };
                format!("({} {op} {})", show(ast, left), show(ast, right))
            }
            Expr::Function { name, args, distinct } => {
                let distinct = if distinct { "DISTINCT " } else { "" };
                format!("{}({distinct}{})", ast.name_text(name), list(args))
            }
            Expr::Cast { operand, ty, try_cast } => {
                let word = if try_cast { "TRY_CAST" } else { "CAST" };
                format!("{word}({} AS {})", show(ast, operand), ast.string(ty))
            }
            Expr::Case { operand, arms, otherwise } => {
                let arms = ast
                    .arm_list(arms)
                    .iter()
                    .map(|arm| format!("WHEN {} THEN {}", show(ast, arm.when), show(ast, arm.then)))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("CASE {} {arms} ELSE {} END", show(ast, operand), show(ast, otherwise))
            }
            Expr::Between { operand, low, high, negated } => {
                let not = if negated { "NOT " } else { "" };
                format!(
                    "({not}{} BETWEEN {} AND {})",
                    show(ast, operand),
                    show(ast, low),
                    show(ast, high)
                )
            }
            Expr::In { operand, list: items, negated } => {
                let not = if negated { "NOT " } else { "" };
                format!("({not}{} IN [{}])", show(ast, operand), list(items))
            }
            Expr::List { items } => format!("[{}]", list(items)),
            Expr::Parameter { name } => format!("${}", ast.string(name)),
            Expr::Row { items } => format!("ROW({})", list(items)),
            Expr::Subquery { query } => format!("({})", show_query(ast, query)),
        }
    }

    /// One from item written back out.
    fn show_source(ast: &Ast, source: SourceRef) -> String {
        let alias = |alias: StrRef| match alias {
            NONE => String::new(),
            other => format!(" AS {}", ast.string(other)),
        };
        match ast.source(source) {
            Source::Table { name, alias: name_alias, .. } => {
                format!("{}{}", ast.name_text(name), alias(name_alias))
            }
            Source::Function { name, args, alias: call_alias, .. } => {
                let args = ast
                    .expr_list(args)
                    .iter()
                    .map(|&item| show(ast, item))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}({args}){}", ast.name_text(name), alias(call_alias))
            }
            Source::Subquery { query, alias: query_alias, .. } => {
                format!("({}){}", show_query(ast, query), alias(query_alias))
            }
            Source::Values { rows, alias: values_alias, .. } => {
                format!("{}{}", show_rows(ast, rows), alias(values_alias))
            }
            Source::Join { left, right, kind, natural, on, using } => {
                let natural = if natural { "NATURAL " } else { "" };
                let on = if on == NONE { String::new() } else { format!(" ON {}", show(ast, on)) };
                let using = if using.is_empty() {
                    String::new()
                } else {
                    format!(" USING ({})", ast.name_text(using))
                };
                format!(
                    "({} {natural}{kind:?} JOIN {}{on}{using})",
                    show_source(ast, left),
                    show_source(ast, right)
                )
            }
        }
    }

    /// The rows of a `VALUES` written back out.
    fn show_rows(ast: &Ast, rows: Slice) -> String {
        let rows = ast
            .rows(rows)
            .iter()
            .map(|&row| {
                let items = ast
                    .expr_list(row)
                    .iter()
                    .map(|&item| show(ast, item))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({items})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("VALUES {rows}")
    }

    /// One query written back out.
    fn show_query(ast: &Ast, index: QueryRef) -> String {
        let query = ast.query(index);
        let list = |slice: Slice| {
            ast.expr_list(slice).iter().map(|&item| show(ast, item)).collect::<Vec<_>>().join(", ")
        };
        let mut out = match query.body {
            QueryBody::SetOp { op, quantifier, by_name, left, right } => {
                let by_name = if by_name { " BY NAME" } else { "" };
                format!(
                    "({} {op:?} {quantifier:?}{by_name} {})",
                    show_query(ast, left),
                    show_query(ast, right)
                )
            }
            QueryBody::Select(index) => {
                let select = ast.select(index);
                let distinct = match select.distinct {
                    Distinct::No => String::new(),
                    Distinct::Yes => " DISTINCT".to_string(),
                    Distinct::On(on) => format!(" DISTINCT ON ({})", list(on)),
                };
                let targets = ast
                    .target_list(select.targets)
                    .iter()
                    .map(|target| match target.alias {
                        NONE => show(ast, target.expr),
                        alias => format!("{} AS {}", show(ast, target.expr), ast.string(alias)),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut out = format!("SELECT{distinct} {targets}");
                if !select.from.is_empty() {
                    let from = ast
                        .source_list(select.from)
                        .iter()
                        .map(|&source| show_source(ast, source))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out += &format!(" FROM {from}");
                }
                if select.filter != NONE {
                    out += &format!(" WHERE {}", show(ast, select.filter));
                }
                if select.group_by_all {
                    out += " GROUP BY ALL";
                } else if !select.group_by.is_empty() {
                    out += &format!(" GROUP BY {}", list(select.group_by));
                }
                if select.having != NONE {
                    out += &format!(" HAVING {}", show(ast, select.having));
                }
                out
            }
            QueryBody::Values(rows) => show_rows(ast, rows),
        };
        if query.order_by_all {
            out += " ORDER BY ALL";
        } else if !query.order_by.is_empty() {
            let items = ast
                .order_list(query.order_by)
                .iter()
                .map(|item| format!("{} {:?} {:?}", show(ast, item.expr), item.order, item.nulls))
                .collect::<Vec<_>>()
                .join(", ");
            out += &format!(" ORDER BY {items}");
        }
        if query.limit != NONE {
            let percent = if query.limit_percent { "%" } else { "" };
            out += &format!(" LIMIT {}{percent}", show(ast, query.limit));
        }
        if query.offset != NONE {
            out += &format!(" OFFSET {}", show(ast, query.offset));
        }
        out
    }

    /// One statement, transformed and written back out.
    fn round(query: &str) -> String {
        let ast = parse_ast(query).unwrap_or_else(|error| panic!("{query}: {error}"));
        assert_eq!(ast.statements.len(), 1, "{query} is one statement");
        let Statement::Query(index) = ast.statements[0] else {
            panic!("{query} is not a query");
        };
        show_query(&ast, index)
    }

    /// One statement, transformed and written back out as the DDL and DML shape it is.
    fn round_statement(query: &str) -> String {
        let ast = parse_ast(query).unwrap_or_else(|error| panic!("{query}: {error}"));
        assert_eq!(ast.statements.len(), 1, "{query} is one statement");
        match ast.statements[0] {
            Statement::Query(index) => show_query(&ast, index),
            Statement::CreateTable(index) => {
                let create = ast.create_table(index);
                let mut out = "CREATE".to_string();
                if create.or_replace {
                    out += " OR REPLACE";
                }
                if create.temporary {
                    out += " TEMPORARY";
                }
                out += " TABLE";
                if create.if_not_exists {
                    out += " IF NOT EXISTS";
                }
                out += &format!(" {}", ast.name_text(create.name));
                let columns = ast
                    .column_defs(create.columns)
                    .iter()
                    .map(|def| {
                        let ty = match def.ty {
                            NONE => String::new(),
                            other => format!(" {}", ast.string(other)),
                        };
                        let null = if def.not_null { " NOT NULL" } else { "" };
                        format!("{}{ty}{null}", ast.string(def.name))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if !columns.is_empty() || create.query == NONE {
                    out += &format!(" ({columns})");
                }
                if create.query != NONE {
                    out += &format!(" AS {}", show_query(&ast, create.query));
                }
                out
            }
            Statement::CreateView(index) => {
                let create = ast.create_view(index);
                let mut out = "CREATE".to_string();
                if create.or_replace {
                    out += " OR REPLACE";
                }
                if create.temporary {
                    out += " TEMPORARY";
                }
                out += " VIEW";
                if create.if_not_exists {
                    out += " IF NOT EXISTS";
                }
                out += &format!(" {}", ast.name_text(create.name));
                if !create.columns.is_empty() {
                    let columns = ast.name(create.columns).collect::<Vec<_>>().join(", ");
                    out += &format!(" ({columns})");
                }
                out + &format!(" AS {}", show_query(&ast, create.query))
            }
            Statement::DropTable(index) => {
                let drop = ast.drop_table(index);
                let mut out = if drop.view { "DROP VIEW" } else { "DROP TABLE" }.to_string();
                if drop.if_exists {
                    out += " IF EXISTS";
                }
                let names = ast
                    .name_list(drop.names)
                    .iter()
                    .map(|&name| ast.name_text(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                out + &format!(" {names}")
            }
            Statement::Insert(index) => {
                let insert = ast.insert(index);
                let mut out = format!("INSERT INTO {}", ast.name_text(insert.name));
                if !insert.columns.is_empty() {
                    let columns = ast.name(insert.columns).collect::<Vec<_>>().join(", ");
                    out += &format!(" ({columns})");
                }
                out + &format!(" {}", show_query(&ast, insert.source))
            }
        }
    }

    #[test]
    fn the_query_m0_has_to_run_transforms() {
        assert_eq!(round("SELECT * FROM t WHERE x > 5"), "SELECT * FROM t WHERE (x Gt 5)");
    }

    #[test]
    fn a_create_table_keeps_its_types_as_text() {
        assert_eq!(
            round_statement("CREATE TABLE t (a INTEGER, b VARCHAR NOT NULL)"),
            "CREATE TABLE t (a INTEGER, b VARCHAR NOT NULL)"
        );
        // The type is the text between the identifier and whatever follows it, parentheses and
        // all, because resolving `DECIMAL(18, 3)` into a width and a scale is the binder's job and
        // doing it here would mean two places that know the type table.
        assert_eq!(
            round_statement("CREATE TABLE t (a DECIMAL(18, 3), b STRUCT(x INT))"),
            "CREATE TABLE t (a DECIMAL(18, 3), b STRUCT(x INT))"
        );
    }

    #[test]
    fn the_modifiers_on_a_create_table_survive() {
        assert_eq!(
            round_statement("CREATE OR REPLACE TEMPORARY TABLE s.t (a INT)"),
            "CREATE OR REPLACE TEMPORARY TABLE s.t (a INT)"
        );
        assert_eq!(
            round_statement("CREATE TEMPORARY TABLE IF NOT EXISTS s.t (a INT)"),
            "CREATE TEMPORARY TABLE IF NOT EXISTS s.t (a INT)"
        );
    }

    #[test]
    fn or_replace_and_if_not_exists_in_one_statement_is_refused_here_and_not_later() {
        // The grammar has room for both and duckdb's has not, so its refusal is a parser error with
        // a caret under the `NOT` and this one is a parser error at the same stage. It is the same
        // sentence whatever is being created.
        for sql in [
            "CREATE OR REPLACE TABLE IF NOT EXISTS t (a INT)",
            "CREATE OR REPLACE VIEW IF NOT EXISTS v AS SELECT 1",
        ] {
            let error = parse_ast(sql).unwrap_err().to_string();
            assert_eq!(
                error,
                "Parser Error: Cannot specify both OR REPLACE and IF NOT EXISTS within single \
                 create statement"
            );
        }
    }

    #[test]
    fn a_create_table_as_carries_the_query_and_not_the_types() {
        assert_eq!(
            round_statement("CREATE TABLE t AS SELECT a FROM u"),
            "CREATE TABLE t AS SELECT a FROM u"
        );
        // The names are the syntax's to say and the types are the query's, so the column
        // definitions here have names and no types.
        assert_eq!(
            round_statement("CREATE TABLE t (x, y) AS SELECT a, b FROM u"),
            "CREATE TABLE t (x, y) AS SELECT a, b FROM u"
        );
    }

    #[test]
    fn a_create_view_carries_its_body_twice_over() {
        assert_eq!(
            round_statement("CREATE VIEW v AS SELECT a FROM u"),
            "CREATE VIEW v AS SELECT a FROM u"
        );
        assert_eq!(
            round_statement("CREATE OR REPLACE VIEW main.v (x, y) AS SELECT a, b FROM u"),
            "CREATE OR REPLACE VIEW main.v (x, y) AS SELECT a, b FROM u"
        );
        // The text the catalog keeps is the body and only the body, so that binding it again is
        // binding a query rather than a `CREATE` statement.
        let ast = parse_ast("CREATE VIEW v (x) AS SELECT a FROM u WHERE a > 1").expect("parses");
        let Statement::CreateView(index) = ast.statements[0] else {
            panic!("not a create view");
        };
        assert_eq!(ast.string(ast.create_view(index).sql), "SELECT a FROM u WHERE a > 1");
    }

    #[test]
    fn a_drop_view_is_not_a_drop_table() {
        assert_eq!(round_statement("DROP VIEW IF EXISTS a, b"), "DROP VIEW IF EXISTS a, b");
        assert_eq!(round_statement("DROP TABLE a"), "DROP TABLE a");
    }

    #[test]
    fn a_drop_table_is_a_list_of_qualified_names() {
        assert_eq!(round_statement("DROP TABLE t"), "DROP TABLE t");
        assert_eq!(round_statement("DROP TABLE IF EXISTS a, b.c"), "DROP TABLE IF EXISTS a, b.c");
    }

    #[test]
    fn dropping_something_that_is_neither_a_table_nor_a_view_is_refused() {
        // `TableOrView` covers `MATERIALIZED VIEW` as well, which is not a thing this database has,
        // and dropping one as if it were an ordinary view is a wrong answer rather than a missing
        // feature.
        let error = parse_ast("DROP MATERIALIZED VIEW v").unwrap_err().to_string();
        assert!(error.starts_with("Not implemented Error"), "{error}");
    }

    #[test]
    fn both_spellings_of_insert_arrive_at_a_query() {
        assert_eq!(
            round_statement("INSERT INTO t VALUES (1, 'a'), (2, 'b')"),
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')"
        );
        assert_eq!(
            round_statement("INSERT INTO t (a, b) SELECT x, y FROM u"),
            "INSERT INTO t (a, b) SELECT x, y FROM u"
        );
    }

    #[test]
    fn an_insert_clause_that_changes_the_answer_is_refused() {
        for query in [
            "INSERT INTO t VALUES (1) RETURNING *",
            "INSERT OR REPLACE INTO t VALUES (1)",
            "INSERT INTO t BY NAME SELECT 1 AS a",
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
            "INSERT INTO t DEFAULT VALUES",
        ] {
            let error = parse_ast(query).unwrap_err().to_string();
            assert!(error.starts_with("Not implemented Error"), "{query} gave {error}");
        }
    }

    #[test]
    fn a_column_constraint_that_is_not_not_null_is_refused() {
        // Nothing enforces a constraint yet. Accepting one and not enforcing it is the wrong
        // answer, so `NOT NULL` is kept because the column already has a nullability and the rest
        // are refused until there is somewhere to put them.
        for query in [
            "CREATE TABLE t (a INT PRIMARY KEY)",
            "CREATE TABLE t (a INT UNIQUE)",
            "CREATE TABLE t (a INT CHECK (a > 0))",
            "CREATE TABLE t (a INT DEFAULT 1)",
            "CREATE TABLE t (a INT REFERENCES u (b))",
            "CREATE TABLE t (a INT, PRIMARY KEY (a))",
        ] {
            let error = parse_ast(query).unwrap_err().to_string();
            assert!(error.starts_with("Not implemented Error"), "{query} gave {error}");
        }
    }

    #[test]
    fn values_is_a_query_on_its_own_and_in_a_from() {
        assert_eq!(round("VALUES (1), (2)"), "VALUES (1), (2)");
        // Parenthesised it is a subquery whose body is the values, and bare it is a `ValuesRef`.
        // Two rules and one meaning, which is the grammar's doing and not something to flatten
        // here, because the parenthesised form can carry an order by and the bare one cannot.
        assert_eq!(
            round("SELECT * FROM (VALUES (1, 2), (3, 4)) t(a, b)"),
            "SELECT * FROM (VALUES (1, 2), (3, 4)) AS t"
        );
        assert_eq!(
            round("SELECT * FROM VALUES (1, 2), (3, 4) AS t(a, b)"),
            "SELECT * FROM VALUES (1, 2), (3, 4) AS t"
        );
        // Rows of different widths parse. Saying so wants the column count, which for an insert is
        // the table's, so the check belongs to the binder and not here.
        assert_eq!(round("VALUES (1), (2, 3)"), "VALUES (1), (2, 3)");
    }

    #[test]
    fn every_statement_in_the_corpus_gets_a_defined_answer() {
        // The point of the test is the word defined. Forty of these are statement kinds and
        // clauses this milestone does not cover, and the requirement is not that they work, it is
        // that they fail by saying so. A panic, a silently dropped clause or an internal error
        // would each be a different bug and all three would be invisible without this.
        let mut done = 0;
        for query in CORPUS {
            match parse_ast(query) {
                Ok(ast) => {
                    assert_eq!(ast.statements.len(), 1, "{query}");
                    done += 1;
                }
                Err(error) => {
                    let message = error.to_string();
                    assert!(
                        message.starts_with("Not implemented Error"),
                        "{query} failed with {message}, which is not a not-implemented error"
                    );
                }
            }
        }
        // Not an assertion about the right number. It is a ratchet: this only moves up, and the
        // day it moves down somebody has taken a construct out without meaning to.
        assert!(done >= 22, "only {done} of the corpus transforms, which is fewer than it was");
    }

    #[test]
    fn the_ast_is_far_smaller_than_the_parse_tree() {
        let query = CORPUS[4];
        let tree = parse(query).unwrap();
        let ast = parse_ast(query).unwrap();
        // The twenty precedence levels are the difference. Every one of them is a node in the
        // parse tree for every expression at every depth, and none of them survives into the AST.
        assert!(
            ast.node_count() * 20 < tree.arena_len(),
            "{} ast nodes against {} parse nodes",
            ast.node_count(),
            tree.arena_len()
        );
    }

    #[test]
    fn precedence_comes_out_of_the_chain_and_into_the_tree() {
        assert_eq!(round("SELECT 1 + 2 * 3"), "SELECT (1 Add (2 Multiply 3))");
        assert_eq!(round("SELECT (1 + 2) * 3"), "SELECT ((1 Add 2) Multiply 3)");
        assert_eq!(round("SELECT 1 + 2 + 3"), "SELECT ((1 Add 2) Add 3)");
        assert_eq!(round("SELECT 1 - 2 - 3"), "SELECT ((1 Subtract 2) Subtract 3)");
        assert_eq!(
            round("SELECT a OR b AND c"),
            "SELECT (a Or (b And c))",
            "and binds tighter than or"
        );
    }

    #[test]
    fn a_double_negation_is_two_nodes_and_not_none() {
        // Folding it would be an optimizer decision and this is not the optimizer. It also would
        // not be safe in general: `NOT NOT x` on a null is still null and on a non boolean it is
        // still an error, and both of those have to survive to the binder to be reported.
        assert_eq!(round("SELECT NOT NOT a"), "SELECT (Not (Not a))");
    }

    #[test]
    fn a_parenthesised_single_expression_is_not_a_row() {
        assert_eq!(round("SELECT (a)"), "SELECT a");
        assert_eq!(round("SELECT (a, b)"), "SELECT ROW(a, b)");
    }

    #[test]
    fn a_bracketed_list_is_a_list_of_however_many_items_were_written() {
        // One item is a list of one, which is where this parts company with the parenthesised form
        // above: `(a)` is `a` and `[a]` is a list, because the brackets are what say list.
        assert_eq!(round("SELECT [a]"), "SELECT [a]");
        assert_eq!(round("SELECT [1, 2, 3]"), "SELECT [1, 2, 3]");
        assert_eq!(round("SELECT []"), "SELECT []");
        assert_eq!(round("SELECT ['a.parquet', 'b.parquet']"), "SELECT ['a.parquet', 'b.parquet']");
    }

    #[test]
    fn a_parameter_carries_its_identifier_however_it_was_written() {
        assert_eq!(round("SELECT $1"), "SELECT $1");
        assert_eq!(round("SELECT ?1"), "SELECT $1");
        assert_eq!(round("SELECT $name"), "SELECT $name");
        // A bare question mark is numbered by where it is, and the counting is its own, so a later
        // `$2` does not push the first one along. This is duckdb v1.4.1, which prints `$1 + $2`.
        assert_eq!(round("SELECT ? + $2"), "SELECT ($1 Add $2)");
        assert_eq!(round("SELECT ?, ?, ?"), "SELECT $1, $2, $3");
    }

    #[test]
    fn the_parameters_of_a_statement_are_listed_once_each_in_written_order() {
        let ast = parse_ast("SELECT $b, $a, $b WHERE $a").expect("parses");
        assert_eq!(ast.parameters(), vec!["b", "a"]);
        assert!(parse_ast("SELECT 1").expect("parses").parameters().is_empty());
    }

    #[test]
    fn the_three_ways_to_write_an_alias_all_arrive() {
        assert_eq!(round("SELECT a AS b"), "SELECT a AS b");
        assert_eq!(round("SELECT a b"), "SELECT a AS b");
        assert_eq!(round("SELECT b: a"), "SELECT a AS b");
        assert_eq!(round("SELECT a"), "SELECT a", "and no alias when none was written");
    }

    #[test]
    fn a_from_with_no_select_selects_everything() {
        // DuckDB's own shorthand. Inventing the star here rather than in the binder means the
        // binder never has to know that the clause it is looking at was the one that was missing.
        assert_eq!(round("FROM t"), "SELECT * FROM t");
        assert_eq!(round("FROM t SELECT a"), "SELECT a FROM t");
    }

    #[test]
    fn joins_nest_to_the_left() {
        assert_eq!(
            round("SELECT * FROM a JOIN b ON a.i = b.i LEFT JOIN c USING (k)"),
            "SELECT * FROM ((a Inner JOIN b ON (a.i Eq b.i)) Left JOIN c USING (k))"
        );
        assert_eq!(
            round("SELECT * FROM a NATURAL JOIN b"),
            "SELECT * FROM (a NATURAL Inner JOIN b)"
        );
        assert_eq!(round("SELECT * FROM a CROSS JOIN b"), "SELECT * FROM (a Cross JOIN b)");
        assert_eq!(
            round("SELECT * FROM a POSITIONAL JOIN b"),
            "SELECT * FROM (a Positional JOIN b)"
        );
        assert_eq!(round("SELECT * FROM a, b"), "SELECT * FROM a, b", "a comma is not a join node");
    }

    #[test]
    fn a_qualified_name_keeps_its_parts_however_it_was_spelled() {
        // Five grammar rules can produce a column reference and they disagree about which
        // component is a schema and which is a table. None of that is decidable without the
        // catalog, so the AST holds the parts and the binder decides.
        assert_eq!(round("SELECT a"), "SELECT a");
        assert_eq!(round("SELECT t.a"), "SELECT t.a");
        assert_eq!(round("SELECT s.t.a"), "SELECT s.t.a");
        assert_eq!(round("SELECT c.s.t.a"), "SELECT c.s.t.a");
        assert_eq!(round("SELECT * FROM s.t"), "SELECT * FROM s.t");
    }

    #[test]
    fn a_star_can_be_qualified() {
        assert_eq!(round("SELECT *"), "SELECT *");
        assert_eq!(round("SELECT t.*"), "SELECT t.*");
        assert_eq!(round("SELECT s.t.*"), "SELECT s.t.*");
    }

    #[test]
    fn a_quoted_identifier_keeps_its_case_and_loses_its_quotes() {
        // DuckDB does not fold identifier case at any point, quoted or not, which the tokenizer
        // work established by reading the source. So the only thing to do here is take the quotes
        // off and resolve the doubled ones.
        let ast = parse_ast("SELECT \"Mixed Case\", \"a\"\"b\"").unwrap();
        assert_eq!(ast.strings[0], "Mixed Case");
        assert_eq!(ast.strings[1], "a\"b");
    }

    #[test]
    fn a_string_literal_is_decoded_and_adjacent_ones_are_joined() {
        assert_eq!(round("SELECT 'it''s'"), "SELECT 'it's'");
        assert_eq!(round("SELECT 'a'\n'b'"), "SELECT 'ab'", "the standard's adjacency rule");
    }

    #[test]
    fn the_null_and_boolean_tests_are_postfix_unary_operators() {
        assert_eq!(round("SELECT x IS NULL"), "SELECT (IsNull x)");
        assert_eq!(round("SELECT x IS NOT NULL"), "SELECT (IsNotNull x)");
        assert_eq!(round("SELECT x ISNULL"), "SELECT (IsNull x)");
        assert_eq!(round("SELECT x NOTNULL"), "SELECT (IsNotNull x)");
        assert_eq!(round("SELECT x IS TRUE"), "SELECT (IsTrue x)");
        assert_eq!(round("SELECT x IS NOT FALSE"), "SELECT (IsNotFalse x)");
        assert_eq!(round("SELECT x IS DISTINCT FROM y"), "SELECT (x IsDistinctFrom y)");
        assert_eq!(round("SELECT x IS NOT DISTINCT FROM y"), "SELECT (x IsNotDistinctFrom y)");
    }

    #[test]
    fn the_like_family_folds_its_negation_into_the_operator() {
        assert_eq!(round("SELECT x LIKE 'a'"), "SELECT (x Like 'a')");
        assert_eq!(round("SELECT x NOT LIKE 'a'"), "SELECT (x NotLike 'a')");
        assert_eq!(round("SELECT x ILIKE 'a'"), "SELECT (x ILike 'a')");
        assert_eq!(round("SELECT x ~~ 'a'"), "SELECT (x Like 'a')", "the operator spelling");
        assert_eq!(round("SELECT x !~~ 'a'"), "SELECT (x NotLike 'a')");
        assert_eq!(round("SELECT x SIMILAR TO 'a'"), "SELECT (x SimilarTo 'a')");
        // Glob has no negated operator to fold into, so the negation stays where it was written.
        assert_eq!(round("SELECT x NOT GLOB 'a'"), "SELECT (Not (x Glob 'a'))");
    }

    #[test]
    fn between_and_in_carry_their_negation_as_a_flag() {
        assert_eq!(round("SELECT x BETWEEN 1 AND 2"), "SELECT (x BETWEEN 1 AND 2)");
        assert_eq!(round("SELECT x NOT BETWEEN 1 AND 2"), "SELECT (NOT x BETWEEN 1 AND 2)");
        assert_eq!(round("SELECT x IN (1, 2)"), "SELECT (x IN [1, 2])");
        assert_eq!(round("SELECT x NOT IN (1, 2)"), "SELECT (NOT x IN [1, 2])");
    }

    #[test]
    fn both_spellings_of_a_cast_are_the_same_node() {
        assert_eq!(round("SELECT CAST(x AS BIGINT)"), "SELECT CAST(x AS BIGINT)");
        assert_eq!(round("SELECT x::BIGINT"), "SELECT CAST(x AS BIGINT)");
        assert_eq!(round("SELECT TRY_CAST(x AS BIGINT)"), "SELECT TRY_CAST(x AS BIGINT)");
        assert_eq!(
            round("SELECT x::DECIMAL(18, 3)"),
            "SELECT CAST(x AS DECIMAL(18, 3))",
            "the type is kept as text because parsing it is the type system's job"
        );
    }

    #[test]
    fn a_case_keeps_its_arms_in_order() {
        assert_eq!(
            round("SELECT CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END"),
            "SELECT CASE - WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END"
        );
        assert_eq!(
            round("SELECT CASE x WHEN 1 THEN 'a' END"),
            "SELECT CASE x WHEN 1 THEN 'a' ELSE - END",
            "a simple case keeps the operand and a missing else is not an implicit null yet"
        );
    }

    #[test]
    fn a_field_access_and_a_method_call_are_ordinary_function_calls() {
        // Which is what DuckDB makes of them too. Giving each its own AST node would mean the
        // binder needs a rule for something the function resolver already handles.
        assert_eq!(round("SELECT (f(x)).y"), "SELECT struct_extract(f(x), 'y')");
        assert_eq!(round("SELECT a[1]"), "SELECT array_extract(a, 1)");
    }

    #[test]
    fn an_aggregate_keeps_its_distinct() {
        assert_eq!(round("SELECT count(*)"), "SELECT count(*)");
        assert_eq!(round("SELECT count(DISTINCT x)"), "SELECT count(DISTINCT x)");
        assert_eq!(round("SELECT count(ALL x)"), "SELECT count(x)");
        assert_eq!(round("SELECT main.count(x)"), "SELECT main.count(x)");
    }

    #[test]
    fn the_modifiers_hang_off_the_query_and_not_off_the_select() {
        // `a UNION b ORDER BY x` sorts the union. Putting the order by on the select would have
        // made that unrepresentable, which is why the grammar puts it outside the chain and why
        // the AST follows.
        assert_eq!(
            round("SELECT 1 UNION ALL SELECT 2 ORDER BY 1"),
            "(SELECT 1 Union All SELECT 2) ORDER BY 1 Unstated Unstated"
        );
        assert_eq!(
            round("SELECT a FROM t UNION SELECT b FROM u EXCEPT SELECT c FROM v"),
            "((SELECT a FROM t Union Unstated SELECT b FROM u) Except Unstated SELECT c FROM v)",
            "set operators are left associative"
        );
        assert_eq!(
            round("SELECT 1 UNION SELECT 2 INTERSECT SELECT 3"),
            "(SELECT 1 Union Unstated (SELECT 2 Intersect Unstated SELECT 3))",
            "and intersect binds tighter than the other two"
        );
    }

    #[test]
    fn the_sort_and_limit_clauses_keep_what_was_written() {
        assert_eq!(
            round("SELECT a FROM t ORDER BY a"),
            "SELECT a FROM t ORDER BY a Unstated Unstated"
        );
        assert_eq!(
            round("SELECT a FROM t ORDER BY a DESC NULLS LAST"),
            "SELECT a FROM t ORDER BY a Descending Last"
        );
        assert_eq!(round("SELECT a FROM t ORDER BY ALL"), "SELECT a FROM t ORDER BY ALL");
        assert_eq!(round("SELECT a FROM t GROUP BY ALL"), "SELECT a FROM t GROUP BY ALL");
        assert_eq!(round("SELECT a FROM t LIMIT 10 OFFSET 5"), "SELECT a FROM t LIMIT 10 OFFSET 5");
        assert_eq!(round("SELECT a FROM t OFFSET 5 LIMIT 10"), "SELECT a FROM t LIMIT 10 OFFSET 5");
        assert_eq!(round("SELECT a FROM t LIMIT 10%"), "SELECT a FROM t LIMIT 10%");
        assert_eq!(round("SELECT a FROM t LIMIT ALL"), "SELECT a FROM t", "which is no limit");
    }

    #[test]
    fn a_subquery_appears_in_both_places_it_can() {
        assert_eq!(
            round("SELECT * FROM (SELECT x FROM t) AS s"),
            "SELECT * FROM (SELECT x FROM t) AS s"
        );
        assert_eq!(round("SELECT (SELECT 1)"), "SELECT (SELECT 1)");
    }

    #[test]
    fn distinct_on_keeps_its_expressions() {
        assert_eq!(round("SELECT DISTINCT a"), "SELECT DISTINCT a");
        assert_eq!(round("SELECT ALL a"), "SELECT a", "which is the default written out");
        assert_eq!(round("SELECT DISTINCT ON (a, b) a"), "SELECT DISTINCT ON (a, b) a");
    }

    #[test]
    fn an_operator_the_dialect_does_not_name_is_kept_by_name() {
        // The grammar text says `OperatorLiteral <- Identifier`, which reads as though any bare
        // word could be written infix. It cannot. That rule is one of the 24 the matcher overrides
        // and it is overridden to the bare operator matcher, so what it takes is a run of operator
        // characters. Believing the body here would have produced a transformer that accepted
        // `a foo b`, which DuckDB rejects.
        assert_eq!(round("SELECT a <=> b"), "SELECT (a <=> b)");
        assert!(parse_ast("SELECT a foo b").is_err(), "a bare word is not an operator");
    }

    #[test]
    fn a_script_is_a_list_of_statements() {
        let ast = parse_ast("SELECT 1; SELECT 2;").unwrap();
        assert_eq!(ast.statements.len(), 2);
        // A trailing semicolon makes an empty top level statement in the parse tree, because the
        // grammar's `Statement? (';'+ / EndOfInput)` is happy with nothing on both sides. It is
        // dropped here rather than pretended away in the matcher.
        let Statement::Query(second) = ast.statements[1] else {
            panic!("the second statement is a query");
        };
        assert_eq!(show_query(&ast, second), "SELECT 2");
    }

    #[test]
    fn an_unsupported_construct_names_itself_and_what_was_written() {
        let error = parse_ast("ALTER TABLE t ADD COLUMN a INTEGER").unwrap_err().to_string();
        assert!(error.starts_with("Not implemented Error"), "{error}");
        assert!(error.contains("ALTER TABLE t ADD COLUMN a INTEGER"), "{error}");
        assert!(error.contains("AlterStatement"), "{error}");
    }

    #[test]
    fn a_long_construct_is_cut_short_in_the_message() {
        let query = format!("ALTER TABLE t ADD COLUMN {} INTEGER", "a".repeat(80));
        let error = parse_ast(&query).unwrap_err().to_string();
        assert!(error.contains("..."), "{error}");
        assert!(error.len() < 200, "{error}");
    }

    #[test]
    fn the_transformer_never_panics_on_anything_the_matcher_accepts() {
        // The matcher accepts a good deal that means nothing, because the grammar does. Every one
        // of these parses and none of them is a statement this milestone covers, and the contract
        // is that the answer is an error either way.
        for query in [
            "SELECT",
            "FROM t SELECT",
            "SELECT * FROM t WHERE",
            "SELECT ()",
            "SELECT a FROM t GROUP BY ()",
        ] {
            let answer = parse_ast(query);
            if let Err(error) = answer {
                let message = error.to_string();
                assert!(
                    message.starts_with("Not implemented Error")
                        || message.starts_with("Parser Error"),
                    "{query} failed with {message}"
                );
            }
        }
    }

    #[test]
    fn a_file_name_in_a_from_clause_is_a_table_name_with_the_quotes_off() {
        // Both spellings have to arrive as the same name, because the binder decides whether it is
        // a file by looking at the name, and `'hits.parquet'` with the quotes still on it is not
        // a path that anything can open.
        assert_eq!(round("SELECT * FROM 'hits.parquet'"), "SELECT * FROM hits.parquet");
        assert_eq!(round("SELECT * FROM \"hits.parquet\""), "SELECT * FROM hits.parquet");
        assert_eq!(round("SELECT * FROM 'hits.parquet' AS h"), "SELECT * FROM hits.parquet AS h");
    }

    #[test]
    fn a_function_call_in_a_from_clause_is_a_source_and_not_an_expression() {
        assert_eq!(round("SELECT * FROM range(3)"), "SELECT * FROM range(3)");
        assert_eq!(round("SELECT * FROM range(1, 10, 2)"), "SELECT * FROM range(1, 10, 2)");
        assert_eq!(round("SELECT * FROM main.range(3)"), "SELECT * FROM main.range(3)");
        assert_eq!(round("SELECT * FROM range(3) AS t"), "SELECT * FROM range(3) AS t");
        // The grammar allows a call with no arguments here and the transformer keeps it, because
        // whether a particular function takes none is the binder's question and not this one's.
        assert_eq!(round("SELECT * FROM some_function()"), "SELECT * FROM some_function()");
    }

    #[test]
    fn the_forms_of_a_table_function_this_does_not_cover_are_turned_away_by_name() {
        for query in [
            "SELECT * FROM range(3) WITH ORDINALITY",
            "SELECT * FROM LATERAL range(3)",
            "SELECT * FROM t: range(3)",
        ] {
            let error = parse_ast(query).unwrap_err().to_string();
            assert!(error.contains("grammar rule"), "{query} failed with {error}");
        }
    }

    #[test]
    fn interning_means_a_name_written_twice_is_stored_once() {
        let ast = parse_ast("SELECT a, a, a FROM t WHERE a = a").unwrap();
        assert_eq!(ast.strings.iter().filter(|text| *text == "a").count(), 1);
    }
}
