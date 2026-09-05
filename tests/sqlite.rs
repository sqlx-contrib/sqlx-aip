//! End-to-end tests against a real in-memory SQLite database.
//!
//! `tests/postgres.rs` covers the numbered-placeholder path. This covers the
//! positional one, which is not the same code: a `?` cannot point back at an
//! earlier bind, so the key-set predicate repeats each cursor value once per
//! clause that pins it. Getting that wrong shifts every bind by one and
//! silently resumes the page from the wrong row — no error, no failed
//! assertion on the SQL text, just a page that is quietly incorrect.
//!
//! SQLite needs no service, so unlike the Postgres tests these always run.

#![cfg(feature = "sqlite")]

use aip::{CursorValue, OrderBy, PageToken};
use sqlx::{AssertSqlSafe, Row, SqlitePool};
use sqlx_aip::{BindAll, Columns, Query, QueryFragment, dialect};

const COLUMNS: Columns<'static> = Columns::new(&[
    ("name", "volumes.id"),
    ("title", "volumes.title"),
    ("read_count", "volumes.read_count"),
]);

/// Eight rows over three distinct titles. The duplicates are the point: an
/// ordering of `title` alone cannot page these, and `title, name` can.
async fn seeded_pool() -> SqlitePool {
    let pool = SqlitePool::connect(":memory:").await.unwrap();
    sqlx::query(
        "CREATE TABLE volumes (
            id         INTEGER PRIMARY KEY,
            title      TEXT    NOT NULL,
            read_count INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    for (id, title, read_count) in [
        (1i64, "Dune", 10i64),
        (2, "Dune", 3),
        (3, "Dune", 7),
        (4, "Emma", 1),
        (5, "Emma", 12),
        (6, "Ubik", 4),
        (7, "Ubik", 9),
        (8, "Ubik", 2),
    ] {
        sqlx::query("INSERT INTO volumes VALUES (?, ?, ?)")
            .bind(id)
            .bind(title)
            .bind(read_count)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

/// Runs one page of a `List` request and returns the ids it served.
async fn page(pool: &SqlitePool, query: &Query<'_>, page_size: i64) -> Vec<i64> {
    let QueryFragment {
        where_sql,
        order_sql,
        values,
    } = query.rewrite(dialect::Sqlite).expect("must rewrite");

    let where_clause = where_sql.map_or(String::new(), |sql| format!("WHERE {sql}"));
    let order_clause = order_sql.map_or(String::new(), |sql| format!("ORDER BY {sql}"));
    let sql = format!("SELECT id FROM volumes {where_clause} {order_clause} LIMIT ?");

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
async fn cursor_for(pool: &SqlitePool, id: i64, order_by: &OrderBy) -> Vec<CursorValue> {
    let row = sqlx::query("SELECT id, title FROM volumes WHERE id = ?")
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

/// Walks every page and returns the ids in the order they were served.
async fn walk(pool: &SqlitePool, spec: &str, filter: Option<&str>, page_size: i64) -> Vec<i64> {
    let order_by: OrderBy = spec.parse().unwrap();
    let mut token = PageToken::default();
    let mut seen = Vec::new();

    loop {
        let query = Query {
            filter: filter.map(|source| cel::Program::compile(source).unwrap()),
            order_by: order_by.clone(),
            page_token: token.clone(),
            columns: COLUMNS,
        };
        let ids = page(pool, &query, page_size).await;
        if ids.is_empty() {
            break;
        }
        let last = *ids.last().unwrap();
        seen.extend(ids);
        token = token
            .next_cursor(cursor_for(pool, last, &order_by).await)
            .unwrap();
    }
    seen
}

/// The test the fragment-level assertions cannot stand in for. Three ordering
/// fields bind six values here and three on Postgres; if the repeat is wrong,
/// this is what notices.
#[tokio::test]
async fn pages_through_a_non_unique_sort_column_exactly_once() {
    let pool = seeded_pool().await;
    assert_eq!(
        walk(&pool, "title, name", None, 3).await,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
    );
}

/// The same walk with a descending leading column, because `<` and `>` are
/// chosen per field and only one of them is exercised above.
#[tokio::test]
async fn pages_through_a_descending_column_exactly_once() {
    let pool = seeded_pool().await;
    assert_eq!(
        walk(&pool, "title desc, name", None, 3).await,
        vec![6, 7, 8, 4, 5, 1, 2, 3],
    );
}

/// A filter and a cursor in the same query. On a positional dialect the two
/// value lists are simply concatenated in bind order, and the filter's literals
/// have to come first or every `?` after them reads the wrong value.
#[tokio::test]
async fn a_filter_and_a_cursor_share_one_bind_sequence() {
    let pool = seeded_pool().await;
    // Excludes ids 2, 4 and 8; leaves 1, 3, 5, 6, 7 in title/id order.
    assert_eq!(
        walk(&pool, "title, name", Some("read_count > 3"), 2).await,
        vec![1, 3, 5, 6, 7],
    );
}

/// A top-level `OR` in the filter must not absorb the cursor's `AND`.
#[tokio::test]
async fn a_top_level_or_in_the_filter_stays_parenthesised() {
    let pool = seeded_pool().await;
    let query = Query {
        filter: Some(cel::Program::compile(r#"read_count > 8 || title == "Dune""#).unwrap()),
        order_by: "name".parse().unwrap(),
        page_token: PageToken::default()
            .next_cursor(vec![CursorValue::Int(5)])
            .unwrap(),
        columns: COLUMNS,
    };
    // Matching rows are 1 ("Dune", 10), 2, 3 ("Dune") and 7 (read_count 9).
    // Only 7 is past the cursor; ids 1-3 coming back would mean the `OR` had
    // escaped its parentheses.
    assert_eq!(page(&pool, &query, 10).await, vec![7]);
}

/// Every `Value` variant SQLite can bind, actually encoded and compared.
#[tokio::test]
async fn cursor_values_encode_on_sqlite() {
    let pool = seeded_pool().await;

    for (spec, cursor, expected) in [
        ("name", vec![CursorValue::Int(6)], vec![7i64, 8]),
        (
            "title, name",
            vec![CursorValue::String("Emma".to_owned()), CursorValue::Int(5)],
            vec![6, 7, 8],
        ),
    ] {
        let query = Query {
            filter: None,
            order_by: spec.parse().unwrap(),
            page_token: PageToken {
                cursor,
                ..PageToken::default()
            },
            columns: COLUMNS,
        };
        assert_eq!(page(&pool, &query, 10).await, expected, "for {spec}");
    }
}

/// The fail-closed gate does not depend on the dialect.
#[tokio::test]
async fn an_unmapped_column_never_reaches_the_database() {
    let query = Query {
        filter: Some(cel::Program::compile(r#"internal_notes == "secret""#).unwrap()),
        order_by: OrderBy::default(),
        page_token: PageToken::default(),
        columns: COLUMNS,
    };
    assert!(matches!(
        query.rewrite(dialect::Sqlite).unwrap_err(),
        sqlx_aip::Error::UnknownField { .. },
    ));
}
