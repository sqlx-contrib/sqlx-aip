# The rewrite contract

This is the normative specification for the crate's one entry point. It is a
port of [pgxaip](https://github.com/pgx-contrib/pgxaip) — read `query.go`
there first; it is 194 lines and this crate is the same shape.

Keyset cursors are specified separately in [cursor.md](cursor.md), and the
filter is not specified here at all: it belongs to
[sqlx-cel](https://github.com/sqlx-contrib/sqlx-cel), which this crate calls.

## What the crate is

Glue. It takes the three query dimensions of an AIP List request, already
parsed by [aip-rs](https://github.com/protoc-contrib/aip-rs) and the code
`protoc-gen-rust-aip` generates, and returns SQL fragments the caller splices
into a query it wrote by hand.

It does not build a `SELECT`, does not talk to a database, and does not own
the transpiler. Roughly 200 lines when finished. Resist every temptation to
make it a query builder.

```rust
let query = sqlx_aip::Query {
    filter: request.parse_filter()?,        // Option<cel::Program>
    order_by: request.parse_order_by()?,    // aip::OrderBy
    page_token: request.parse_page_token()?,// aip::PageToken
    columns: VOLUME_COLUMNS,
};

let sqlx_aip::QueryFragment { where_sql, order_sql, values } = query.rewrite()?;
```

## The three outputs

| Field | Contents | When empty |
| --- | --- | --- |
| `where_sql` | The filter predicate, the cursor predicate, or both `AND`ed | Neither present — caller omits the `WHERE` keyword entirely |
| `order_sql` | `"col" ASC, "col" DESC`, **without** the `ORDER BY` prefix | `order_by` has no fields — the server's choice of order, not an error |
| `values` | `Vec<sqlx_cel::Value>`, numbered `$1..$N`: filter literals first, then cursor values | No filter literals and no cursor |

Emptiness is signalled by an empty string, matching pgxaip. Consider
`Option<String>` instead — it makes "omit the clause" unmissable in the
caller's code, where `if !where_sql.is_empty()` is easy to forget and produces
`WHERE ` followed by nothing. This is a small, real improvement over the Go
API and the port is the moment to take it.

The caller appends its own `LIMIT` / `OFFSET` at `$N+1`:

```rust
let sql = format!(
    "SELECT * FROM volumes {} {} LIMIT ${} OFFSET ${}",
    where_clause, order_clause, values.len() + 1, values.len() + 2,
);
```

## Composition order

Fixed, because the parameter numbering depends on it:

1. Transpile the filter with `param_offset = 1`.
2. Build the cursor predicate with `param_offset = 1 + filter_values.len()`.
3. Rewrite `order_by`, which binds nothing.
4. Join: `(filter) AND cursor`, with the filter parenthesised because it may
   be a bare `OR` at the top level.

Concatenate the value vectors in the same order. `query.go:51` is the whole
function.

## `order_by`

`aip::OrderBy` is already parsed and already validated against
`QUERY_FIELDS` by the generated `parse_order_by`. This crate does the second,
narrower check: every `OrderByField::path` must be in the column map, or the
rewrite fails.

```
aip::OrderBy { fields: [{ path: "title", desc: false },
                        { path: "create_time", desc: true }] }
→  "volumes"."title" ASC, "volumes"."created_at" DESC
```

Direction is `ASC` unless `desc`. Quoting is sqlx-cel's, per segment. No
`NULLS FIRST` / `NULLS LAST` is emitted — see [cursor.md](cursor.md), where
that omission stops being cosmetic.

## `page_token`

`PageToken::offset` is **not consulted**. The caller feeds it to its own
`OFFSET` clause. This is deliberate in the Go version and worth keeping: the
crate never decides pagination strategy, it only renders what the token
already committed to.

`PageToken::cursor`, when non-empty, produces the keyset predicate.

## Errors

One `#[non_exhaustive]` enum. The variants that matter are distinguishable
because they say different things about the caller:

- **an unmapped path** in the filter, the ordering, or the cursor — a
  configuration bug in the column map, or a client probing a field the proto
  declares but the map withholds
- **a cursor arity mismatch** — `cursor.len() != order_by.fields.len()`, which
  means a token was issued under a different ordering
- **a null cursor value** — see [cursor.md](cursor.md)
- **anything from sqlx-cel** — wrapped, with the source preserved

All of them are `InvalidArgument` at the RPC boundary except the unmapped-path
case, which is arguably `Internal` when the map is wrong and
`InvalidArgument` when the client is probing. The crate cannot tell them
apart; do not pretend otherwise in the error type.

Hand-write `Display` and `Error`, as `aip-rs` does. `aip::QueryError` is the
model — including `source()` returning the wrapped error.

## Relationship to aip-rs

`aip-rs` deliberately has zero dependencies, including no CEL crate and no
database crate. That is why this repository exists: it is where the AIP
runtime types meet Postgres, and it depends on both so that neither has to
depend on the other.

Consume `aip::OrderBy`, `aip::PageToken` and `aip::CursorValue` as given. If
something is awkward to consume, the fix probably belongs upstream in
`aip-rs` — raise it there rather than adding a conversion shim here.
