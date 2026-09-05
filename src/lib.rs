//! Rewrites the `filter`, `order_by` and `page_token` of an AIP `List`
//! request into Postgres SQL fragments.
//!
//! Not implemented yet. The design lives in `docs/`, and
//! [pgxaip](https://github.com/pgx-contrib/pgxaip) is the reference
//! implementation this one ports:
//!
//! - `docs/query.md` — the rewrite contract and `order_by`
//! - `docs/cursor.md` — the keyset predicate and null cursor values

#![deny(missing_docs)]
