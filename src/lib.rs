//! Renders the `filter`, `order_by` and `page_token` of an AIP `List` request
//! into SQL fragments, for [sqlx](https://github.com/launchbadge/sqlx).
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use aip::PageToken;
//! use sqlx_aip::{Columns, Query, QueryFragment, QueryRenderer, dialect};
//!
//! // Once per resource: which database, and which paths may reach which
//! // columns. Neither changes from one request to the next.
//! const VOLUMES: QueryRenderer<'static, dialect::Postgres> =
//!     QueryRenderer::new(dialect::Postgres).columns(Columns::new(&[
//!         ("name", "volumes.id"),
//!         ("title", "volumes.title"),
//!         ("read_count", "volumes.read_count"),
//!     ]));
//!
//! // Per request: what the client asked for.
//! let query = Query {
//!     filter: Some(cel::Program::compile("read_count > 3")?),
//!     // The trailing `name` is the primary key. See "Stability", below.
//!     order_by: "title, name".parse()?,
//!     page_token: PageToken::default(),
//! };
//!
//! let QueryFragment { where_sql, order_sql, values } = VOLUMES.render(&query)?;
//!
//! assert_eq!(where_sql.as_deref(), Some(r#""volumes"."read_count" > $1"#));
//! assert_eq!(
//!     order_sql.as_deref(),
//!     Some(r#""volumes"."title" ASC, "volumes"."id" ASC"#),
//! );
//! # Ok(())
//! # }
//! ```
//!
//! The split is the point: [`Query`] is the request and nothing else, so it is
//! a field-by-field copy of what `protoc-gen-rust-aip` generates, and the call
//! site passes one argument.
//!
//! # What this crate is
//!
//! Glue. It takes the three query dimensions of a `List` request, already
//! parsed by [aip-rs](https://github.com/protoc-contrib/aip-rs) and the code
//! [protoc-gen-rust-aip](https://github.com/protoc-contrib/protoc-gen-rust-aip)
//! generates, and returns fragments the caller splices into a query it wrote by
//! hand. It does not build a `SELECT`, does not talk to a database, and does
//! not own the transpiler — the `filter` goes to
//! [sqlx-cel](https://github.com/sqlx-contrib/sqlx-cel).
//!
//! Each fragment is [`None`] when it has nothing to say, so omitting the
//! keyword is the caller's obvious move rather than something to remember:
//!
//! ```
//! # use sqlx_aip::QueryFragment;
//! # let fragment = QueryFragment { where_sql: None, order_sql: None, values: Vec::new() };
//! let where_clause = fragment.where_sql.map_or(String::new(), |sql| format!("WHERE {sql}"));
//! let order_clause = fragment.order_sql.map_or(String::new(), |sql| format!("ORDER BY {sql}"));
//!
//! // The values are in bind order, so the caller's own bindings follow them.
//! let sql = format!(
//!     "SELECT * FROM volumes {where_clause} {order_clause} LIMIT ${} OFFSET ${}",
//!     fragment.values.len() + 1,
//!     fragment.values.len() + 2,
//! );
//! ```
//!
//! Bind them with [`BindAll`], and wrap the assembled string in
//! `AssertSqlSafe`: the only caller-influenced text in a fragment is column
//! names, and those come from a fail-closed map rather than from request data.
//!
#![cfg_attr(feature = "postgres", doc = "```no_run")]
#![cfg_attr(not(feature = "postgres"), doc = "```ignore")]
//! # async fn example(pool: sqlx::PgPool, page_size: i64) -> Result<(), Box<dyn std::error::Error>> {
//! # let fragment = sqlx_aip::QueryFragment { where_sql: None, order_sql: None, values: Vec::new() };
//! # let sql = String::new();
//! use sqlx::AssertSqlSafe;
//! use sqlx_aip::BindAll;
//!
//! let rows = sqlx::query(AssertSqlSafe(sql))
//!     .bind_all(fragment.values)
//!     .bind(page_size)
//!     .fetch_all(&pool)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Splicing into a query that already binds
//!
//! Above, the fragment's placeholders come first and the caller's `LIMIT` and
//! `OFFSET` follow them. The other arrangement is at least as common — a
//! generated query with its own parameters, and a filter spliced into a
//! sentinel comment somewhere in the middle of it — and for that, tell the
//! renderer where to start with [`at`](QueryRenderer::at):
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # use sqlx_aip::{Columns, Query, QueryRenderer, dialect};
//! # const VOLUMES: QueryRenderer<'static, dialect::Postgres> = QueryRenderer::new(dialect::Postgres)
//! #     .columns(Columns::new(&[("read_count", "read_count")]));
//! # let query = Query { filter: Some(cel::Program::compile("read_count > 3")?), ..Default::default() };
//! // The statement binds $1 and $2 already, so the fragment starts at $3.
//! let fragment = VOLUMES.at(3).render(&query)?;
//!
//! assert_eq!(fragment.where_sql.as_deref(), Some(r#""read_count" > $3"#));
//! # Ok(())
//! # }
//! ```
//!
//! The alternative is renumbering the fragment after the fact, which means
//! scanning SQL for `$N` while stepping over the string literals a `LIKE`
//! fragment carries. Nobody should have to write that scanner to paginate a
//! table, so the offset exists to make sure nobody does.
//!
//! # Dialects
//!
//! [`QueryRenderer::new`] takes the same [`Dialect`] sqlx-cel does, so the SQL
//! is whatever flavour the caller asks for — see [`dialect`] for what varies.
//!
//! One thing does not merely change shape between them. A numbered placeholder
//! can be referenced from several places and bound once; a positional `?`
//! cannot, and the key-set predicate pins each more-significant column in every
//! clause after the first. So the *same* ordering produces a different number
//! of bind values:
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # use aip::{CursorValue, PageToken};
//! # use sqlx_aip::{Columns, Query, QueryRenderer, dialect};
//! # const COLUMNS: Columns<'static> = Columns::new(&[("title", "title"), ("id", "id")]);
//! let query = Query {
//!     filter: None,
//!     order_by: "title, id".parse()?,
//!     page_token: PageToken {
//!         cursor: vec![CursorValue::String("Dune".into()), CursorValue::Int(7)],
//!         ..PageToken::default()
//!     },
//! };
//!
//! // ("title" > $1) OR ("title" = $1 AND "id" > $2)
//! let postgres = QueryRenderer::new(dialect::Postgres).columns(COLUMNS);
//! assert_eq!(postgres.render(&query)?.values.len(), 2);
//! // ("title" > ?)  OR ("title" = ?  AND "id" > ?)
//! let sqlite = QueryRenderer::new(dialect::Sqlite).columns(COLUMNS);
//! assert_eq!(sqlite.render(&query)?.values.len(), 3);
//! # Ok(())
//! # }
//! ```
//!
//! [`values`](QueryFragment::values) is always in bind order, so a caller that
//! hands the whole list to [`BindAll`] never has to know which it got.
//!
//! # Stability is the caller's job
//!
//! **A key-set cursor is only stable if the ordering ends in a unique column.**
//! Append the primary key to [`OrderBy::fields`](aip::OrderBy::fields) before
//! rendering, and make sure the cursor carries a matching trailing value.
//!
//! Neither this crate nor `aip-rs` enforces it. Without a unique tiebreaker,
//! rows that share the leading sort key have no defined order between pages, so
//! a page can repeat rows it already served and skip ones it never did —
//! forever, with no error anywhere. The arity check in
//! [`Error::CursorArity`] is the only thing standing between a caller and that
//! outcome, and it cannot see the difference.
//!
//! This is the single most likely way to use the crate wrongly.
//!
//! # Ordering columns must be `NOT NULL`
//!
//! A [`CursorValue::Null`](aip::CursorValue::Null) is rejected with
//! [`Error::NullCursorValue`] rather than bound. Three things break at once if
//! it is not: `col > $1` with a NULL bind evaluates to NULL rather than true so
//! the row is dropped; `col = $1` is NULL too so every clause after the first
//! is dead; and a database's default null ordering does not agree with a
//! null-aware predicate unless the `ORDER BY` carries an explicit
//! `NULLS FIRST` / `NULLS LAST`, which this crate does not emit. Handling nulls
//! correctly needs all of that plus a per-column type hint, since a NULL must
//! be typed before it can be sent. An error is the honest answer.
//!
//! # `page_token.offset` is not consulted
//!
//! [`PageToken::offset`](aip::PageToken::offset) is handed back untouched, for
//! the caller to feed to its own `OFFSET`. The crate never decides a pagination
//! strategy; it renders what the token already committed to.
//!
//! # Scope
//!
//! **In.** `order_by` → an `ORDER BY` list. A key-set cursor → the compound
//! predicate. Delegating `filter` to sqlx-cel. Fail-closed path → column
//! resolution across all three.
//!
//! **Out.** Parsing, building a `SELECT`, deciding `LIMIT` / `OFFSET`, and
//! anything else that would make this a query builder.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// A `CursorValue::Timestamp` has to become a `Value::Timestamp`, and that
// variant only exists when sqlx-cel has a date-time backend. Without one the
// mapping would be partial and the crate would need an error variant for a
// configuration nobody wants, so the configuration is refused instead.
#[cfg(not(any(feature = "chrono", feature = "time")))]
compile_error!(
    "sqlx-aip needs one of the `chrono` or `time` features: a Timestamp cursor value has nowhere to go without one"
);

mod cursor;
mod error;
mod order;

pub use error::{Dimension, Error};

// Re-exported so that a caller who takes `values` from a [`QueryFragment`] does
// not have to add sqlx-cel to their own manifest to name one, pick a dialect,
// or bind the result.
pub use sqlx_cel::{Columns, Dialect, Options, Value, dialect};

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
pub use sqlx_cel::BindAll;

/// The parsed query dimensions of a `List` request.
///
/// One field per dimension, and nothing else: this is what varies from one
/// request to the next. What does not — the dialect, the column map, where
/// placeholder numbering starts — belongs to the [`QueryRenderer`] that reads
/// it.
///
/// The three parsers come from `protoc-gen-rust-aip`, which generates them onto
/// the request type as a struct with exactly these fields, so going from one to
/// the other is a field-by-field copy. Nothing stops a caller building an
/// [`OrderBy`](aip::OrderBy) and a [`PageToken`](aip::PageToken) by hand.
///
/// `order_by` is the *effective* ordering — whatever the client asked for, plus
/// the tiebreaker the caller appends. See "Stability is the caller's job" in
/// the crate docs.
///
/// Every field is absent on a first unfiltered page, so [`Default`] is usually
/// the shortest way to build one.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # use sqlx_aip::Query;
/// let query = Query {
///     order_by: "name".parse()?,
///     ..Default::default()
/// };
/// # Ok(())
/// # }
/// ```
// Not `Clone`: `cel::Program` is not.
#[derive(Debug, Default)]
pub struct Query {
    /// The compiled AIP-160 `filter`, or [`None`] when the request carried
    /// none.
    pub filter: Option<cel::Program>,
    /// The effective AIP-132 ordering.
    pub order_by: aip::OrderBy,
    /// The decoded AIP-158 page token. Only its
    /// [`cursor`](aip::PageToken::cursor) is read.
    pub page_token: aip::PageToken,
}

/// The SQL fragments and bind values a [`QueryRenderer`] produces from a
/// [`Query`].
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # use sqlx_aip::{Columns, Query, QueryFragment, QueryRenderer, dialect};
/// # const VOLUMES: QueryRenderer<'static, dialect::Postgres> = QueryRenderer::new(dialect::Postgres)
/// #     .columns(Columns::new(&[("title", "volumes.title")]));
/// let QueryFragment { where_sql, order_sql, values } = VOLUMES.render(&Query::default())?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct QueryFragment {
    /// The `WHERE` predicate — the filter, the cursor, or the two `AND`ed —
    /// without the `WHERE` keyword.
    ///
    /// [`None`] when there is neither a filter nor a cursor, in which case the
    /// caller omits the keyword entirely.
    pub where_sql: Option<String>,

    /// The comma-separated ordering terms, without the `ORDER BY` prefix.
    ///
    /// [`None`] when the ordering has no fields. That is the server's choice of
    /// order, not an error.
    pub order_sql: Option<String>,

    /// The bind values, in the order they must be bound: the filter's literals
    /// first, then the cursor's.
    ///
    /// The caller's own `LIMIT` / `OFFSET` values follow them. How many there
    /// are depends on the dialect — see "Dialects" in the crate docs.
    pub values: Vec<Value>,
}

/// Renders a [`Query`] into a [`QueryFragment`], for one database and one
/// resource.
///
/// Everything a render needs that is *not* the request lives here: which
/// flavour of SQL to emit, which AIP paths may reach which columns, and where
/// placeholder numbering starts. Those are properties of the table and the
/// deployment rather than of the call, so they are stated once and the call
/// site passes only the request:
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sqlx_aip::{Columns, Query, QueryRenderer, dialect};
///
/// const VOLUMES: QueryRenderer<'static, dialect::Postgres> =
///     QueryRenderer::new(dialect::Postgres).columns(Columns::new(&[
///         ("title", "volumes.title"),
///         ("name", "volumes.id"),
///     ]));
///
/// let fragment = VOLUMES.render(&Query {
///     order_by: "title, name".parse()?,
///     ..Default::default()
/// })?;
///
/// assert_eq!(
///     fragment.order_sql.as_deref(),
///     Some(r#""volumes"."title" ASC, "volumes"."id" ASC"#),
/// );
/// # Ok(())
/// # }
/// ```
///
/// `new` is `const`, so a renderer is a `const` beside the query it serves. It
/// is [`Copy`] and holds no allocation — the dialects are zero-sized and
/// [`Columns`] borrows its entries.
///
/// # Why the dialect is not an [`Options`] field
///
/// It has no default. An options struct that carries one either gives up
/// [`Default`] — and with it the struct-update syntax that makes options
/// pleasant — or defaults to some dialect, and then a MySQL caller who writes
/// `..Default::default()` silently gets `$1` placeholders and `"ident"`
/// quoting. Requiring it in the constructor makes that unrepresentable.
#[derive(Debug, Clone, Copy)]
pub struct QueryRenderer<'a, D> {
    dialect: D,
    columns: Columns<'a>,
    param_offset: usize,
}

impl<'a, D: Dialect> QueryRenderer<'a, D> {
    /// A renderer for `dialect`, with an empty column map.
    ///
    /// Empty means every path is rejected, so a renderer straight out of `new`
    /// serves nothing until [`columns`](Self::columns) says what it may serve.
    /// That is the intended shape of the default, not an oversight: the column
    /// map is the security boundary, and the failure of forgetting it is a
    /// loud [`Error::UnknownField`] on the first request rather than a query
    /// that quietly exposes a field.
    ///
    /// The dialect is not optional in the same way. It has no correct default,
    /// and a wrong one is not an error at all — it is `$1` placeholders sent
    /// to MySQL, or `"ident"` quoting where backticks were needed. So it is
    /// the one thing the constructor insists on.
    #[must_use]
    pub const fn new(dialect: D) -> Self {
        Self {
            dialect,
            // `Columns::default()` is not const-callable; this is the same
            // empty map.
            columns: Columns::new(&[]),
            param_offset: 1,
        }
    }

    /// The same renderer, resolving AIP paths through `columns`.
    ///
    /// This is the security boundary, and it governs all three dimensions. A
    /// CEL environment generated from a proto declares every field of the
    /// resource, so the parser accepts `internal_notes == "x"` quite happily;
    /// the column map is what stops it reaching SQL, and lookup through it is
    /// fail-closed.
    ///
    /// ```
    /// # use sqlx_aip::{Columns, QueryRenderer, dialect};
    /// const VOLUMES: QueryRenderer<'static, dialect::Postgres> =
    ///     QueryRenderer::new(dialect::Postgres).columns(Columns::new(&[
    ///         ("title", "volumes.title"),
    ///         ("name", "volumes.id"),
    ///     ]));
    /// ```
    #[must_use]
    pub const fn columns(mut self, columns: Columns<'a>) -> Self {
        self.columns = columns;
        self
    }

    /// The same renderer, numbering its placeholders from `offset`.
    ///
    /// `4` emits `$4`, `$5`, … so the fragment splices into a statement that
    /// already binds three parameters of its own. Without it a caller has to
    /// renumber the fragment afterwards, which means scanning SQL for `$N`
    /// while stepping over the string literals a `LIKE` fragment carries — a
    /// scanner nobody should have to write to paginate a table.
    ///
    /// Takes and returns by value, so it composes onto a `const` renderer at
    /// the call site: `VOLUMES.at(3).render(&query)`.
    ///
    /// `0` is treated as `1`, since there is no `$0`.
    ///
    /// # Positional dialects
    ///
    /// A `?` carries no number, so SQLite and MySQL ignore this — but bind
    /// order still decides which value lands where, and there it follows the
    /// *text*. A fragment spliced into the middle of such a statement has to
    /// have its values bound in the middle too, which is a constraint numbered
    /// placeholders do not impose. [`Dialect::is_positional`] is how a caller
    /// asks which it is dealing with.
    #[must_use]
    pub const fn at(mut self, offset: usize) -> Self {
        // `max` is not const-callable on usize in this position.
        self.param_offset = if offset == 0 { 1 } else { offset };
        self
    }

    /// Renders `query` into the SQL fragments and bind values it stands for.
    ///
    /// The composition order is fixed, because the bind order depends on it:
    /// the filter is transpiled first, the cursor predicate is numbered after
    /// the filter's literals, and the value vectors are concatenated to match.
    /// The filter is parenthesised when it is joined to the cursor, because it
    /// may be a bare `OR` at the top level.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] for a path absent from the column map in any of the
    /// three dimensions, for a cursor whose length does not match the ordering,
    /// for a null cursor value, and for anything sqlx-cel rejected in the
    /// filter.
    pub fn render(&self, query: &Query) -> Result<QueryFragment, Error> {
        // Step 1: the filter, from the first placeholder.
        let (filter_sql, mut values) = match &query.filter {
            Some(program) => {
                let fragment = sqlx_cel::transpile_with(
                    program.expression(),
                    self.columns,
                    &self.dialect,
                    Options {
                        param_offset: self.param_offset,
                    },
                )?;
                (Some(fragment.sql), fragment.values)
            }
            None => (None, Vec::new()),
        };

        // Step 2: the cursor, numbered after the filter's literals.
        let (cursor_sql, cursor_values) = cursor::render(
            &query.order_by,
            &query.page_token.cursor,
            self.columns,
            &self.dialect,
            self.param_offset + values.len(),
        )?;
        values.extend(cursor_values);

        // Step 3: the ordering, which binds nothing.
        let order_sql = order::render(&query.order_by, self.columns, &self.dialect)?;

        // Step 4: join. The filter is parenthesised; the cursor predicate
        // brings its own parentheses.
        let where_sql = match (filter_sql, cursor_sql) {
            (Some(filter), Some(cursor)) => Some(format!("({filter}) AND {cursor}")),
            (Some(filter), None) => Some(filter),
            (None, Some(cursor)) => Some(cursor),
            (None, None) => None,
        };

        Ok(QueryFragment {
            where_sql,
            order_sql,
            values,
        })
    }
}

/// Resolves one AIP path through the column map, fail-closed.
///
/// Shared by the ordering and the cursor. The filter's own lookup is
/// sqlx-cel's, and its miss is lifted into the same [`Error::UnknownField`] by
/// the [`From`] impl.
fn column<'a>(columns: Columns<'a>, path: &str, dimension: Dimension) -> Result<&'a str, Error> {
    columns.get(path).ok_or_else(|| Error::UnknownField {
        dimension,
        path: path.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Columns, Error, Query, QueryRenderer, Value, dialect};
    use aip::{CursorValue, OrderBy, PageToken};

    const COLUMNS: Columns<'static> = Columns::new(&[
        ("title", "volumes.title"),
        ("read_count", "volumes.read_count"),
        ("name", "volumes.id"),
    ]);

    /// The two renderers under test, differing only in dialect — which is the
    /// axis most of these assertions are about.
    const POSTGRES: QueryRenderer<'static, dialect::Postgres> =
        QueryRenderer::new(dialect::Postgres).columns(COLUMNS);
    const SQLITE: QueryRenderer<'static, dialect::Sqlite> =
        QueryRenderer::new(dialect::Sqlite).columns(COLUMNS);

    fn query(filter: Option<&str>, order_by: &str, cursor: Vec<CursorValue>) -> Query {
        Query {
            filter: filter.map(|source| cel::Program::compile(source).unwrap()),
            order_by: order_by.parse().unwrap(),
            page_token: PageToken {
                cursor,
                ..PageToken::default()
            },
        }
    }

    #[test]
    fn no_filter_and_no_cursor_is_no_where_clause() {
        let fragment = POSTGRES.render(&query(None, "title", Vec::new())).unwrap();
        assert_eq!(fragment.where_sql, None);
        assert_eq!(
            fragment.order_sql.as_deref(),
            Some(r#""volumes"."title" ASC"#)
        );
        assert_eq!(fragment.values, Vec::new());
    }

    #[test]
    fn a_filter_alone_is_not_parenthesised() {
        let fragment = POSTGRES
            .render(&query(Some("read_count > 3"), "", Vec::new()))
            .unwrap();
        assert_eq!(
            fragment.where_sql.as_deref(),
            Some(r#""volumes"."read_count" > $1"#),
        );
        assert_eq!(fragment.order_sql, None);
        assert_eq!(fragment.values, vec![Value::Int(3)]);
    }

    /// The reason for the parentheses: a top-level `OR` would otherwise bind
    /// looser than the `AND` joining it to the cursor, and widen the page to
    /// rows before it.
    #[test]
    fn a_filter_is_parenthesised_before_the_cursor_is_anded_on() {
        let fragment = POSTGRES
            .render(&query(
                Some(r#"read_count > 3 || title == "Dune""#),
                "name",
                vec![CursorValue::Int(7)],
            ))
            .unwrap();
        assert_eq!(
            fragment.where_sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."read_count" > $1 OR "volumes"."title" = $2))"#,
                r#" AND (("volumes"."id" > $3))"#,
            )),
        );
    }

    /// The composition order is the contract: the filter's literals are bound
    /// first and the cursor's follow, in exactly that order in `values`.
    #[test]
    fn the_cursors_placeholders_follow_the_filters() {
        let fragment = POSTGRES
            .render(&query(
                Some(r#"read_count > 3 && title != "Dune""#),
                "title, name",
                vec![CursorValue::String("Emma".to_owned()), CursorValue::Int(7)],
            ))
            .unwrap();
        assert_eq!(
            fragment.where_sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."read_count" > $1 AND "volumes"."title" != $2))"#,
                r#" AND (("volumes"."title" > $3)"#,
                r#" OR ("volumes"."title" = $3 AND "volumes"."id" > $4))"#,
            )),
        );
        assert_eq!(
            fragment.values,
            vec![
                Value::Int(3),
                Value::Text("Dune".to_owned()),
                Value::Text("Emma".to_owned()),
                Value::Int(7),
            ],
        );
    }

    /// The point of the offset: the fragment is numbered for a query that
    /// already binds parameters, so nothing downstream has to renumber it.
    #[test]
    fn an_offset_moves_the_first_placeholder() {
        let fragment = POSTGRES
            .at(4)
            .render(&query(Some("read_count > 3"), "", Vec::new()))
            .unwrap();
        assert_eq!(
            fragment.where_sql.as_deref(),
            Some(r#""volumes"."read_count" > $4"#),
        );
    }

    /// The offset shifts both dimensions, and the cursor still follows the
    /// filter's literals -- so `values` is in bind order whatever the offset.
    #[test]
    fn an_offset_moves_the_cursor_along_with_the_filter() {
        let fragment = POSTGRES
            .at(3)
            .render(&query(
                Some(r#"read_count > 3 && title != "Dune""#),
                "title, name",
                vec![CursorValue::String("Emma".to_owned()), CursorValue::Int(7)],
            ))
            .unwrap();
        assert_eq!(
            fragment.where_sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."read_count" > $3 AND "volumes"."title" != $4))"#,
                r#" AND (("volumes"."title" > $5)"#,
                r#" OR ("volumes"."title" = $5 AND "volumes"."id" > $6))"#,
            )),
        );
        assert_eq!(
            fragment.values,
            vec![
                Value::Int(3),
                Value::Text("Dune".to_owned()),
                Value::Text("Emma".to_owned()),
                Value::Int(7),
            ],
        );
    }

    /// There is no `$0`. sqlx-cel says so for the filter; the cursor is
    /// numbered from the same offset and has to agree.
    #[test]
    fn a_zero_offset_starts_where_the_default_does() {
        let zero = POSTGRES
            .at(0)
            .render(&query(
                Some("read_count > 3"),
                "name",
                vec![CursorValue::Int(7)],
            ))
            .unwrap();
        let default = POSTGRES
            .render(&query(
                Some("read_count > 3"),
                "name",
                vec![CursorValue::Int(7)],
            ))
            .unwrap();

        assert_eq!(zero, default);
        assert_eq!(
            zero.where_sql.as_deref(),
            Some(r#"("volumes"."read_count" > $1) AND (("volumes"."id" > $2))"#),
        );
    }

    /// A `?` carries no number, so there is nothing for the offset to move.
    /// Bind order is unchanged, which is the part that still matters there.
    #[test]
    fn a_positional_dialect_ignores_the_offset() {
        let shifted = SQLITE
            .at(9)
            .render(&query(
                Some("read_count > 3"),
                "name",
                vec![CursorValue::Int(7)],
            ))
            .unwrap();
        let default = SQLITE
            .render(&query(
                Some("read_count > 3"),
                "name",
                vec![CursorValue::Int(7)],
            ))
            .unwrap();

        assert_eq!(shifted, default);
    }

    /// The same query on a positional dialect: identical value *order*, but the
    /// cursor's repeat because a `?` cannot point back at an earlier bind.
    #[test]
    fn a_positional_dialect_keeps_bind_order_and_repeats_what_it_must() {
        let fragment = SQLITE
            .render(&query(
                Some(r#"read_count > 3 && title != "Dune""#),
                "title, name",
                vec![CursorValue::String("Emma".to_owned()), CursorValue::Int(7)],
            ))
            .unwrap();
        assert_eq!(
            fragment.where_sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."read_count" > ? AND "volumes"."title" != ?))"#,
                r#" AND (("volumes"."title" > ?)"#,
                r#" OR ("volumes"."title" = ? AND "volumes"."id" > ?))"#,
            )),
        );
        assert_eq!(
            fragment.values,
            vec![
                Value::Int(3),
                Value::Text("Dune".to_owned()),
                Value::Text("Emma".to_owned()),
                Value::Text("Emma".to_owned()),
                Value::Int(7),
            ],
        );
    }

    #[test]
    fn a_cursor_alone_is_the_whole_where_clause() {
        let fragment = POSTGRES
            .render(&query(None, "name", vec![CursorValue::Int(7)]))
            .unwrap();
        assert_eq!(
            fragment.where_sql.as_deref(),
            Some(r#"(("volumes"."id" > $1))"#),
        );
        assert_eq!(fragment.values, vec![Value::Int(7)]);
    }

    /// The column map governs the filter too, and its miss is the same variant
    /// the ordering's is.
    #[test]
    fn an_unmapped_filter_path_never_reaches_sql() {
        let error = POSTGRES
            .render(&query(
                Some(r#"internal_notes == "secret""#),
                "",
                Vec::new(),
            ))
            .unwrap_err();
        assert_eq!(
            error,
            Error::UnknownField {
                dimension: super::Dimension::Filter,
                path: "internal_notes".to_owned(),
            },
        );
    }

    /// Anything else sqlx-cel rejects arrives wrapped, source intact.
    #[test]
    fn an_untranslatable_filter_is_wrapped() {
        let error = POSTGRES
            .render(&query(Some("read_count + 1 > 3"), "", Vec::new()))
            .unwrap_err();
        assert!(matches!(error, Error::Filter(_)), "{error:?}");
        assert!(core::error::Error::source(&error).is_some());
    }

    /// The offset belongs to the caller's `OFFSET` clause, and must not leak
    /// into the predicate or the values.
    #[test]
    fn the_tokens_offset_is_not_consulted() {
        let mut query = query(None, "name", Vec::new());
        query.page_token.offset = 250;
        let fragment = POSTGRES.render(&query).unwrap();
        assert_eq!(fragment.where_sql, None);
        assert_eq!(fragment.values, Vec::new());
    }

    #[test]
    fn a_cursor_issued_under_a_different_ordering_is_rejected() {
        let error = POSTGRES
            .render(&query(None, "title, name", vec![CursorValue::Int(7)]))
            .unwrap_err();
        assert_eq!(
            error,
            Error::CursorArity {
                fields: 2,
                values: 1
            },
        );
    }

    /// An empty ordering is legitimate on its own, but there is nothing for a
    /// cursor to resume from.
    #[test]
    fn an_empty_ordering_cannot_carry_a_cursor() {
        let error = POSTGRES
            .render(&query(None, "", vec![CursorValue::Int(7)]))
            .unwrap_err();
        assert_eq!(
            error,
            Error::CursorArity {
                fields: 0,
                values: 1
            },
        );
    }

    /// An empty map rejects everything, which is what fail-closed means -- and
    /// a renderer that was never given one has an empty map, so forgetting
    /// [`QueryRenderer::columns`] fails loudly on the first request rather than
    /// serving a field it should not.
    #[test]
    fn a_renderer_with_no_columns_rejects_every_ordering() {
        let query = Query {
            order_by: "title".parse::<OrderBy>().unwrap(),
            ..Default::default()
        };

        assert!(
            QueryRenderer::new(dialect::Postgres)
                .render(&query)
                .is_err()
        );
    }
}
