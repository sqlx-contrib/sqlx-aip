# sqlx-aip

> Rewrite a Google AIP `List` request — `filter`, `order_by` and `page_token` —
> into SQL fragments with bind values, behind a fail-closed column allow-list.

[![CI](https://github.com/sqlx-contrib/sqlx-aip/actions/workflows/ci.yml/badge.svg)](https://github.com/sqlx-contrib/sqlx-aip/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/sqlx-aip)](https://crates.io/crates/sqlx-aip)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Rewrites the query dimensions of a [Google AIP](https://google.aip.dev) `List`
request into SQL fragments, for [sqlx](https://github.com/launchbadge/sqlx).

The Rust counterpart of [pgxaip](https://github.com/pgx-contrib/pgxaip), built
on [sqlx-cel](https://github.com/sqlx-contrib/sqlx-cel). It is where
[aip-rs](https://github.com/protoc-contrib/aip-rs) meets a database, which is
why it exists as its own crate: `aip-rs` has no dependencies and intends to
keep it that way.

```rust
use sqlx::AssertSqlSafe;
use sqlx_aip::{BindAll, Columns, Query, QueryFragment, dialect};

let query = Query {
    filter: request.parse_filter()?,          // Option<cel::Program>
    order_by: request.parse_order_by()?,      // aip::OrderBy
    page_token: request.parse_page_token()?,  // aip::PageToken
    columns: VOLUME_COLUMNS,
};

let QueryFragment { where_sql, order_sql, values } = query.rewrite(dialect::Postgres)?;
// where_sql: Some(r#"("volumes"."read_count" > $1) AND (("volumes"."id" > $2))"#)
// order_sql: Some(r#""volumes"."title" ASC, "volumes"."id" ASC"#)

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

## Dialects

`rewrite` takes the same `Dialect` sqlx-cel does — `dialect::Postgres`,
`dialect::Sqlite`, `dialect::MySql`, or your own — and the filter, the ordering
and the cursor predicate all follow it.

One thing does not merely change shape between them. A numbered placeholder can
be referenced from several places and bound once; a positional `?` cannot, and
the key-set predicate pins each more-significant column in every clause after
the first. So the same ordering produces a different number of bind values:

```rust
// ("title" > $1) OR ("title" = $1 AND "id" > $2)   — 2 values
query.rewrite(dialect::Postgres)?;
// ("title" > ?)  OR ("title" = ?  AND "id" > ?)    — 3 values
query.rewrite(dialect::Sqlite)?;
```

`values` is always in bind order, so a caller that hands the whole list to
`bind_all` never has to know which it got.

Dialects are pure text and always available. *Binding* the values needs the
matching Cargo feature — `postgres` (default), `sqlite`, `mysql` — which is
sqlx-cel's, forwarded.

## Stability is the caller's job

**A key-set cursor is only stable if the ordering ends in a unique column.**
Append the primary key to `order_by.fields` before rewriting, and make sure the
cursor carries a matching trailing value — as `name` does above.

Without a unique tiebreaker, rows sharing the leading sort key have no defined
order between pages, so a page can repeat rows it already served and skip ones
it never did, with no error anywhere. Neither this crate nor `aip-rs` enforces
it. This is the single most likely way to use the crate wrongly.

Relatedly, an ordering column must be `NOT NULL`. A null cursor value is
rejected rather than bound, because `col > $1` with a NULL bind evaluates to
NULL and silently drops the row.

## The column map is the security boundary

`Query::columns` is an AIP-path → column allow-list, and lookup is
**fail-closed**: a path that is absent is an error, so an empty map rejects
every request. It governs all three dimensions.

```rust
const VOLUME_COLUMNS: Columns<'static> = Columns::new(&[
    ("name",        "volumes.id"),
    ("title",       "volumes.title"),
    ("read_count",  "volumes.read_count"),
    ("create_time", "volumes.created_at"),
]);
```

This matters because a CEL environment generated from a proto declares *every*
field of the resource, so the parser will happily accept `internal_notes == "x"`.
The column map is what stops it reaching SQL. Note the last entry: the AIP path
and the column name differ, which is why this is a map rather than a set.

## Scope

**In.** `order_by` → an `ORDER BY` list. A key-set cursor → the compound
predicate. Delegating `filter` to sqlx-cel. Fail-closed path → column
resolution across all three.

**Out.** Parsing — `aip-rs` and generated code do that. Building a `SELECT`.
Deciding `LIMIT` / `OFFSET`: `PageToken::offset` is handed back untouched for
the caller to use. Anything that would make this a query builder.

## Design notes

The rationale lives with the code — `cargo doc --open`, or the doc comments on
`Query::rewrite`, `QueryFragment`, `Error` and the `cursor` module.

Two things worth knowing that the API docs do not say. This is a port of
[pgxaip](https://github.com/pgx-contrib/pgxaip); its `query.go` is the
reference for the rewrite, so read that first if you are changing the SQL that
comes out. And it departs from pgxaip in one place on purpose: `where_sql` and
`order_sql` are `Option<String>` rather than the empty string, because
`if !sql.is_empty()` is easy to forget and renders `WHERE ` followed by
nothing.

## Development

sqlx 0.9 declares `rust-version = "1.94"`, so this crate does too.
`rust-toolchain.toml` pins the dev toolchain to 1.95.0, so plain `cargo` picks
the right one even when the machine's default stable is older than the MSRV.

```sh
cargo test --features sqlite,mysql   # all drivers, incl. end-to-end SQLite
cargo clippy --all-targets --features sqlite,mysql
```

`tests/sqlite.rs` runs in-memory and always runs. `tests/postgres.rs` needs a
database and skips without `DATABASE_URL`; both walk a table with a
deliberately non-unique leading sort column and assert that no row is seen
twice and none is skipped, which is the one thing fragment-level assertions
cannot stand in for.

There is a Nix flake and a devcontainer for a batteries-included shell — the
pinned toolchain, the `psql` and `sqlite3` CLIs, and a Postgres to run against:

```sh
devcontainer up --workspace-folder .   # brings up Postgres
nix develop
```

`DATABASE_URL` is defined once, in `.devcontainer/devcontainer.json`. The shell
runs [`devcontainer-env`](https://github.com/devcontainer-env/devcontainer-env),
which reads it and rewrites the compose hostname to the port Docker assigned —
so the same definition is correct inside the container, on the host, and in CI.
Without a devcontainer running it stays unset and the Postgres tests skip.

The devcontainer installs Nix and does the same thing, so "Reopen in Container"
lands in the same environment. The flake exposes only a dev shell: this is a
library crate with no binary, and `buildRustPackage` would want a committed
`Cargo.lock`, which a library deliberately does not have.

## License

[MIT](LICENSE)
