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
let query = sqlx_aip::Query {
    filter: request.parse_filter()?,          // Option<cel::Program>
    order_by: request.parse_order_by()?,      // aip::OrderBy
    page_token: request.parse_page_token()?,  // aip::PageToken
    columns: VOLUME_COLUMNS,                  // AIP path -> DB column, fail-closed
};

let sqlx_aip::Rewritten { where_sql, order_sql, values } = query.rewrite()?;

let volumes = sqlx::query_as::<_, Volume>(AssertSqlSafe(format!(
        "SELECT * FROM volumes WHERE {where_sql} ORDER BY {order_sql} LIMIT ${}",
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

## Status

**Not implemented.** This repository currently holds the design only. The
specification in `docs/` is complete enough to implement against, and every
external API it cites was verified against the published sources rather than
recalled.

| Document | What it settles |
| --- | --- |
| [docs/query.md](docs/query.md) | The rewrite contract, composition order, `order_by`, errors |
| [docs/cursor.md](docs/cursor.md) | The keyset predicate, and the decision to reject null cursor values |

Start with `docs/query.md`. It depends on
[sqlx-cel](https://github.com/sqlx-contrib/sqlx-cel), which is also unbuilt —
build that first, since this crate is roughly 200 lines of glue on top of it.

## Scope

**In.** `order_by` → an `ORDER BY` list. A keyset cursor → the compound
predicate. Delegating `filter` to `sqlx-cel`. Fail-closed path → column
resolution across all three.

**Out.** Parsing — `aip-rs` and generated code do that. Building a `SELECT`.
Deciding `LIMIT` / `OFFSET`: `PageToken::offset` is handed back untouched for
the caller to use. Anything that would make this a query builder.

## License

[MIT](LICENSE)
