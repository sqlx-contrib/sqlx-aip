# sqlx-aip

Rewrites the query dimensions of a
[Google AIP](https://google.aip.dev) `List` request — `filter`, `order_by`
and `page_token` — into Postgres SQL fragments, for
[sqlx](https://github.com/launchbadge/sqlx).

The Rust counterpart of [pgxaip](https://github.com/pgx-contrib/pgxaip). It is
where [aip-rs](https://github.com/protoc-contrib/aip-rs) meets Postgres, which
is why it exists as its own crate: `aip-rs` has no dependencies and intends to
keep it that way.

```rust
use sqlx_aip::{BindAll, Columns, Query, QueryFragment};

// AIP path -> DB column. Fail-closed: a path that is absent is an error.
const VOLUME_COLUMNS: Columns<'static> = Columns::new(&[
    ("name", "volumes.id"),
    ("title", "volumes.title"),
    ("create_time", "volumes.created_at"),
]);

let query = Query {
    filter: request.parse_filter()?,          // Option<cel::Program>
    order_by: request.parse_order_by()?,      // aip::OrderBy
    page_token: request.parse_page_token()?,  // aip::PageToken
    columns: VOLUME_COLUMNS,
};

let QueryFragment { where_sql, order_sql, values } = query.rewrite()?;

// Each fragment is `None` when it has nothing to say, so omitting the keyword
// is the obvious move rather than something to remember.
let where_clause = where_sql.map_or(String::new(), |sql| format!("WHERE {sql}"));
let order_clause = order_sql.map_or(String::new(), |sql| format!("ORDER BY {sql}"));

let volumes = sqlx::query_as::<_, Volume>(AssertSqlSafe(format!(
        "SELECT * FROM volumes {where_clause} {order_clause} LIMIT ${}",
        values.len() + 1)))
    .bind_all(values)
    .bind(page_size)
    .fetch_all(&pool)
    .await?;
```

The three parsers come from
[protoc-gen-rust-aip](https://github.com/protoc-contrib/protoc-gen-rust-aip),
which generates them onto the request type. Nothing stops a caller building
`aip::OrderBy` and `aip::PageToken` by hand.

## Stability is the caller's job

**A keyset cursor is only stable if the ordering ends in a unique column.**
Append the primary key to `order_by.fields` before rewriting, and make sure the
cursor carries a matching trailing value — as `name` does above. Without a
unique tiebreaker, rows sharing the leading sort key have no defined order
between pages, so a page can repeat rows it already served and skip ones it
never did, with no error anywhere. This is the single most likely way to use
the crate wrongly.

Relatedly, an ordering column must be `NOT NULL`. A null cursor value is
rejected rather than bound, because `col > $1` with a NULL bind evaluates to
NULL and silently drops the row.

## Status

Implemented, and specified by `docs/`:

| Document | What it settles |
| --- | --- |
| [docs/query.md](docs/query.md) | The rewrite contract, composition order, `order_by`, errors |
| [docs/cursor.md](docs/cursor.md) | The keyset predicate, and the decision to reject null cursor values |

One deliberate departure from the specification, which `docs/query.md` invites:
`where_sql` and `order_sql` are `Option<String>` rather than the empty string
pgxaip returns.

## Development

The dev shell brings its own toolchain and Postgres:

```sh
nix develop          # or `direnv allow`
pg-start             # a throwaway cluster in ./.pgdata, on port 55432
cargo test
pg-stop
```

`cargo test` runs the unit tests and doctests anywhere. The round-trip tests in
`tests/postgres.rs` need a database and skip without one, so set `DATABASE_URL`
if you are not using `pg-start`:

```sh
DATABASE_URL=postgres://localhost/sqlx_aip_test cargo test
```

They page through a table with a deliberately non-unique leading sort column
and assert that no row is seen twice and none is skipped. An ordering bug of
that kind does not show up in fragment-level assertions, which is why CI runs
them against a real Postgres rather than letting them skip.

## Scope

**In.** `order_by` → an `ORDER BY` list. A keyset cursor → the compound
predicate. Delegating `filter` to `sqlx-cel`. Fail-closed path → column
resolution across all three.

**Out.** Parsing — `aip-rs` and generated code do that. Building a `SELECT`.
Deciding `LIMIT` / `OFFSET`: `PageToken::offset` is handed back untouched for
the caller to use. Anything that would make this a query builder.

## License

[MIT](LICENSE)
