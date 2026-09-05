//! End-to-end tests against a real Postgres.
//!
//! The unit tests assert the SQL text and the value list. These assert the part
//! text cannot: that the fragments parse, that the placeholders line up with the
//! values across the filter/cursor boundary, that every [`Value`] variant
//! encodes, and — the one that matters — that paging through a table with a
//! deliberately non-unique leading sort column sees every row exactly once.
//!
//! Set `DATABASE_URL` to run them. Without it each test skips, because a
//! missing database is a missing environment rather than a failure:
//!
//! ```sh
//! DATABASE_URL=postgres://localhost/sqlx_aip_test cargo test --test postgres
//! ```

use std::str::FromStr as _;

use aip::{CursorValue, OrderBy, PageToken};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, PgPool, Row};
use sqlx_aip::{BindAll, Columns, Query, QueryFragment, Value};

/// `name` is the AIP resource-name field, and maps to the primary key. It is
/// the tiebreaker every ordering in these tests ends with.
const COLUMNS: Columns<'static> = Columns::new(&[
    ("name", "volumes.id"),
    ("title", "volumes.title"),
    ("read_count", "volumes.read_count"),
    ("published", "volumes.published"),
    ("rating", "volumes.rating"),
    ("cover", "volumes.cover"),
    ("create_time", "volumes.created_at"),
    ("duration", "volumes.duration"),
]);

/// Creates `schema`, seeds `volumes` inside it, and returns a pool whose
/// `search_path` points there. Returns `None` when `DATABASE_URL` is unset.
///
/// A schema per test, because `cargo test` runs them concurrently against one
/// database and they would otherwise all be seeding the same table.
async fn pool(schema: &str) -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;

    let admin = PgPool::connect(&url)
        .await
        .expect("DATABASE_URL must connect");
    for statement in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
    ] {
        sqlx::query(AssertSqlSafe(statement))
            .execute(&admin)
            .await
            .unwrap();
    }
    admin.close().await;

    let options = PgConnectOptions::from_str(&url)
        .expect("DATABASE_URL must parse")
        .options([("search_path", schema)]);
    let pool = PgPool::connect_with(options).await.unwrap();

    sqlx::query(
        "CREATE TABLE volumes (
            id         BIGINT           PRIMARY KEY,
            title      TEXT             NOT NULL,
            read_count BIGINT           NOT NULL,
            published  BOOLEAN          NOT NULL,
            rating     DOUBLE PRECISION NOT NULL,
            cover      BYTEA            NOT NULL,
            created_at TIMESTAMPTZ      NOT NULL,
            duration   INTERVAL         NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Eight rows over three distinct titles. The duplicates are the point: an
    // ordering of `title` alone cannot page these, and `title, name` can.
    for (id, title, read_count, published, rating, cover) in [
        (1i64, "Dune", 10i64, false, 0.5f64, 1u8),
        (2, "Dune", 3, true, 1.0, 2),
        (3, "Dune", 7, false, 1.5, 3),
        (4, "Emma", 1, true, 2.0, 4),
        (5, "Emma", 12, false, 2.5, 5),
        (6, "Ubik", 4, true, 3.0, 6),
        (7, "Ubik", 9, false, 3.5, 7),
        (8, "Ubik", 2, true, 4.0, 8),
    ] {
        sqlx::query(
            "INSERT INTO volumes VALUES ($1, $2, $3, $4, $5, $6,
                 TIMESTAMPTZ '2024-01-01 00:00:00Z' + ($1 * INTERVAL '1 day'),
                 INTERVAL '90 minutes')",
        )
        .bind(id)
        .bind(title)
        .bind(read_count)
        .bind(published)
        .bind(rating)
        .bind(vec![cover])
        .execute(&pool)
        .await
        .unwrap();
    }
    Some(pool)
}

/// Seeds a schema named after the calling test, or skips the test body when
/// there is no database to run it against.
macro_rules! pool {
    ($schema:literal) => {
        match pool($schema).await {
            Some(pool) => pool,
            None => {
                eprintln!("skipped: DATABASE_URL is unset");
                return;
            }
        }
    };
}

/// Runs one page of a `List` request and returns the ids it served.
async fn page(pool: &PgPool, query: &Query<'_>, page_size: i64) -> Vec<i64> {
    let QueryFragment {
        where_sql,
        order_sql,
        values,
    } = query.rewrite().expect("must rewrite");

    let where_clause = where_sql.map_or(String::new(), |sql| format!("WHERE {sql}"));
    let order_clause = order_sql.map_or(String::new(), |sql| format!("ORDER BY {sql}"));
    let sql = format!(
        "SELECT id FROM volumes {where_clause} {order_clause} LIMIT ${}",
        values.len() + 1,
    );

    sqlx::query(AssertSqlSafe(sql))
        .bind_all(values)
        .bind(page_size)
        .fetch_all(pool)
        .await
        .expect("query must execute")
        .into_iter()
        .map(|row| row.get::<i64, _>("id"))
        .collect()
}

/// Reads the sort key of one row, as the cursor tuple for `order_by`.
async fn cursor_for(pool: &PgPool, id: i64, order_by: &OrderBy) -> Vec<CursorValue> {
    let row = sqlx::query("SELECT id, title FROM volumes WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    order_by
        .paths()
        .map(|path| match path {
            "name" => CursorValue::Int(row.get::<i64, _>("id")),
            "title" => CursorValue::String(row.get::<String, _>("title")),
            other => unreachable!("unexpected ordering path {other}"),
        })
        .collect()
}

/// The test the fragment-level assertions cannot stand in for: page all the way
/// through a non-unique leading sort column and account for every row.
///
/// Eight rows over three titles, three at a time, is the case that breaks if
/// the equality prefix, the placeholder offsets or the tiebreaker are wrong —
/// and it breaks by repeating or dropping rows, never by erroring.
#[tokio::test]
async fn pages_through_a_non_unique_sort_column_exactly_once() {
    let pool = pool!("ascending_walk");
    let order_by: OrderBy = "title, name".parse().unwrap();

    let mut token = PageToken::default();
    let mut seen: Vec<i64> = Vec::new();

    loop {
        let query = Query {
            filter: None,
            order_by: order_by.clone(),
            page_token: token.clone(),
            columns: COLUMNS,
        };
        let ids = page(&pool, &query, 3).await;
        if ids.is_empty() {
            break;
        }
        let last = *ids.last().unwrap();
        seen.extend(ids);
        token = token
            .next_cursor(cursor_for(&pool, last, &order_by).await)
            .unwrap();
    }

    // Titles ascending, ids ascending within each title. Every row once.
    assert_eq!(seen, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

/// The same walk with a descending leading column, because `<` and `>` are
/// chosen per field and only one of them is exercised above.
#[tokio::test]
async fn pages_through_a_descending_column_exactly_once() {
    let pool = pool!("descending_walk");
    let order_by: OrderBy = "title desc, name".parse().unwrap();

    let mut token = PageToken::default();
    let mut seen: Vec<i64> = Vec::new();

    loop {
        let query = Query {
            filter: None,
            order_by: order_by.clone(),
            page_token: token.clone(),
            columns: COLUMNS,
        };
        let ids = page(&pool, &query, 3).await;
        if ids.is_empty() {
            break;
        }
        let last = *ids.last().unwrap();
        seen.extend(ids);
        token = token
            .next_cursor(cursor_for(&pool, last, &order_by).await)
            .unwrap();
    }

    assert_eq!(seen, vec![6, 7, 8, 4, 5, 1, 2, 3]);
}

/// A filter and a cursor in the same query: the filter's literals are `$1..$N`
/// and the cursor's follow. Mis-numbering them is invisible in a fragment
/// assertion but produces a wrong page — or a type error — here.
#[tokio::test]
async fn a_filter_and_a_cursor_share_one_placeholder_sequence() {
    let pool = pool!("filter_and_cursor");
    let order_by: OrderBy = "title, name".parse().unwrap();

    // Excludes ids 2, 4 and 8; leaves 1, 3, 5, 6, 7 in title/id order.
    let filter = || cel::Program::compile("read_count > 3").unwrap();

    let first = page(
        &pool,
        &Query {
            filter: Some(filter()),
            order_by: order_by.clone(),
            page_token: PageToken::default(),
            columns: COLUMNS,
        },
        2,
    )
    .await;
    assert_eq!(first, vec![1, 3]);

    let token = PageToken::default()
        .next_cursor(cursor_for(&pool, 3, &order_by).await)
        .unwrap();
    let second = page(
        &pool,
        &Query {
            filter: Some(filter()),
            order_by: order_by.clone(),
            page_token: token,
            columns: COLUMNS,
        },
        10,
    )
    .await;
    assert_eq!(second, vec![5, 6, 7]);
}

/// A top-level `OR` in the filter must not absorb the cursor's `AND`.
///
/// Without the parentheses `rewrite` puts around the filter, this page would
/// widen to every row matching the right-hand disjunct, cursor or no cursor.
#[tokio::test]
async fn a_top_level_or_in_the_filter_stays_parenthesised() {
    let pool = pool!("top_level_or");
    let order_by: OrderBy = "name".parse().unwrap();

    let token = PageToken::default()
        .next_cursor(vec![CursorValue::Int(5)])
        .unwrap();
    let ids = page(
        &pool,
        &Query {
            filter: Some(cel::Program::compile(r#"read_count > 8 || title == "Dune""#).unwrap()),
            order_by,
            page_token: token,
            columns: COLUMNS,
        },
        10,
    )
    .await;

    // Matching rows are 1 ("Dune", 10), 2, 3 ("Dune") and 7 (read_count 9).
    // Only 7 is past the cursor. Ids 1-3 coming back would mean the `OR` had
    // escaped its parentheses.
    assert_eq!(ids, vec![7]);
}

/// Every `Value` variant a cursor can produce, actually encoded and compared by
/// the server. `Timestamp` and `Duration` are the ones with a real conversion
/// behind them, and `Duration` is the only signed interval.
#[tokio::test]
async fn every_cursor_value_variant_encodes() {
    let pool = pool!("value_variants");

    for (path, value, expected) in [
        ("name", Value::Int(6), vec![7i64, 8]),
        ("title", Value::Text("Emma".to_owned()), vec![6, 7, 8]),
        ("read_count", Value::Int(9), vec![1, 5]),
        ("published", Value::Bool(false), vec![2, 4, 6, 8]),
        ("rating", Value::Float(3.0), vec![7, 8]),
        ("cover", Value::Bytes(vec![6u8]), vec![7, 8]),
    ] {
        let column = COLUMNS.get(path).unwrap();
        let sql = format!("SELECT id FROM volumes WHERE {column} > $1 ORDER BY id");
        let ids: Vec<i64> = sqlx::query(AssertSqlSafe(sql))
            .bind_all([value.clone()])
            .fetch_all(&pool)
            .await
            .unwrap_or_else(|error| panic!("{path} must query: {error}"))
            .into_iter()
            .map(|row| row.get("id"))
            .collect();
        assert_eq!(ids, expected, "for {path} > {value:?}");
    }

    // The two constructed types, through the real cursor path.
    let order_by: OrderBy = "create_time".parse().unwrap();
    let query = Query {
        filter: None,
        order_by,
        // 2024-01-07, which is id 6's `created_at`, so ids 1..=6 are behind it.
        page_token: PageToken {
            cursor: vec![CursorValue::timestamp(1_704_585_600, 0)],
            ..PageToken::default()
        },
        columns: COLUMNS,
    };
    assert_eq!(page(&pool, &query, 10).await, vec![7, 8]);

    // A negative interval, which is why `PgInterval` is built directly rather
    // than through `std::time::Duration`.
    let ids: Vec<i64> = sqlx::query(AssertSqlSafe(
        "SELECT id FROM volumes WHERE duration + $1 < INTERVAL '0' ORDER BY id".to_owned(),
    ))
    .bind_all([Value::Duration(-120 * 60 * 1_000_000)])
    .fetch_all(&pool)
    .await
    .expect("a negative interval must bind")
    .into_iter()
    .map(|row| row.get("id"))
    .collect();
    assert_eq!(ids.len(), 8);
}

/// The fail-closed gate, checked against a column that really exists: the
/// filter parses, the table has the column, and the rewrite still refuses.
#[tokio::test]
async fn an_unmapped_column_never_reaches_the_database() {
    let query = Query {
        filter: Some(cel::Program::compile(r#"created_at == "x""#).unwrap()),
        order_by: OrderBy::default(),
        page_token: PageToken::default(),
        columns: COLUMNS,
    };
    // `created_at` is the *column*; `create_time` is the path that maps to it.
    // Naming the column rather than the path is exactly the probe the map has
    // to refuse.
    assert!(matches!(
        query.rewrite().unwrap_err(),
        sqlx_aip::Error::UnknownField { .. },
    ));
}
