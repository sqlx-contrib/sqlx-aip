# Keyset cursors

The compound predicate that resumes a page, and the one decision in this port
that the Go predecessor never had to make.

## The predicate

Given an effective ordering and the sort-key values of the last row of the
previous page, emit the tuple comparison, expanded so each column can carry
its own direction:

```sql
("title" > $1)
  OR ("title" = $1 AND "id" > $2)
  OR ("title" = $1 AND "id" = $2 AND "rank" < $3)
```

`>` for an `ASC` field, `<` for `DESC`. Placeholders start at
`param_offset` and run in ordering order. `rewriteCursor` in `pgxaip/query.go:155`
is the implementation; port it directly.

An empty cursor is the first page: emit nothing, bind nothing, no error.

`cursor.len()` must equal `order_by.fields.len()`. A mismatch means the token
was issued under a different ordering and is an error, not something to
truncate to the shorter of the two.

### Why not row-value comparison

Postgres supports `(a, b) > ($1, $2)`, which is shorter and can use a
multicolumn index directly. It only works when every column sorts the same
direction, and AIP-132 lets a client write `title asc, create_time desc`.
Emitting the expanded form always is the right trade: one shape to test, and
the planner handles it acceptably given the index.

If a future version special-cases the uniform-direction case, it must stay
behind a check of every field's `desc` flag, not just the first.

## Stability is the caller's job

The predicate is only stable if the ordering ends in a unique column. Append
the primary key to `order_by.fields` before rewriting, and make sure the
cursor carries a matching trailing value. Neither this crate nor `aip-rs`
enforces it — `aip-rs` says the same thing in `OrderBy::paths`, and the arity
check is the only thing standing between a caller and a page that repeats
rows forever.

Document this loudly in the crate docs. It is the single most likely way to
use the crate wrongly.

## Nulls

**Decided: reject `CursorValue::Null` with an error. Do not bind it.**

`aip::CursorValue` models `Null` explicitly (`aip-rs/src/pagination.rs:69`),
because an unset message field or an unset `optional` field contributes one.
That is strictly more information than Go's `[]any` carried, and it exposes a
hole the Go implementation got to ignore rather than one this port introduces.

Three things break at once:

1. **The comparison silently drops rows.** `col > $1` with a NULL bind
   evaluates to NULL, not true. The row is not returned, and pagination stops
   early with no error anywhere.
2. **The equality prefix is wrong too.** `col = $1` is NULL for a NULL bind,
   so every clause after the first is dead. Correct handling needs
   `IS NOT DISTINCT FROM` for the prefix comparisons.
3. **`ORDER BY` does not agree with the predicate.** Postgres sorts NULLs last
   for `ASC` and first for `DESC` by default. A NULL-aware predicate must be
   paired with explicit `NULLS FIRST` / `NULLS LAST` in the ordering, which
   the `order_by` rewrite does not emit — so fixing the predicate alone would
   produce a *differently* wrong result, which is worse than an error.

And one that has no clean answer at all: **a NULL must be typed before it can
be sent.** `args.add(None::<T>)` forces a choice of `T`, and cel-rust has no
type checker, so there is nothing to derive it from. The same constraint
drives the no-`Null`-variant decision in `sqlx-cel/docs/values.md`.

So: error, with a message that names the column and says the ordering column
must be `NOT NULL`. The error is honest, immediate, and points at the fix.
Silently returning a short page is none of those.

### If this is revisited

It is a coherent feature, just a larger one than it looks. It needs all four:
`IS NOT DISTINCT FROM` on the equality prefix, explicit `NULLS FIRST`/`NULLS
LAST` in `order_sql`, a `nulls` ordering option threaded through from the
caller, and a per-column type hint so the NULL can be bound. Do not do the
first without the rest.

## `CursorValue` to `Value`

Mechanical apart from the null. `aip::CursorValue` widens sized integers to 64
bits and `f32` to `f64` on decode, so the mapping is total:

| `aip::CursorValue` | `sqlx_cel::Value` |
| --- | --- |
| `Bool(b)` | `Bool(b)` |
| `String(s)` | `Text(s)` |
| `Bytes(b)` | `Bytes(b)` |
| `Int(i)` | `Int(i)` |
| `Uint(u)` | `Uint(u)` |
| `Float(f)` | `Float(f)` |
| `Timestamp { seconds, nanos }` | `Timestamp(..)` — already normalized by `aip-rs` |
| `Duration { nanos }` | `Interval(..)` — microseconds, may be negative |
| `Null` | **error** |

Both enums are `#[non_exhaustive]`-shaped in spirit; match exhaustively and
let the compiler find the next variant rather than adding a `_` arm.

The `Duration` case is the one to test: it is signed, and `PgInterval` must be
constructed directly rather than through `std::time::Duration`, which cannot
represent a negative value. `sqlx-cel/docs/values.md` covers it.

## Test vectors

`aip-rs/tests/pagination_vectors.rs` pins the token wire format, and
`aip-rs/docs/page-token.md` is normative for it. This crate does not decode
tokens — it receives a decoded `PageToken` — so it needs none of that. What it
does need is a round-trip integration test against a real Postgres: page
through a table with a deliberately non-unique leading sort column and assert
that no row is seen twice and none is skipped. Ordering bugs of this kind do
not show up in fragment-level assertions.
