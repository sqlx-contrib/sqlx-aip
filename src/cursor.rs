use core::fmt::Write as _;

use aip::{CursorValue, OrderBy, OrderByField};
use sqlx_cel::dialect::Dialect;
use sqlx_cel::{Columns, Value};

use crate::column;
use crate::error::{Dimension, Error};

/// Builds the compound key-set predicate that resumes a page, from an
/// effective ordering and the sort-key tuple of the previous page's last row.
///
/// The emitted predicate is the tuple comparison, expanded so that each column
/// can carry its own direction:
///
/// ```sql
/// (("title" > $1)
///   OR ("title" = $1 AND "id" > $2)
///   OR ("title" = $1 AND "id" = $2 AND "rank" < $3))
/// ```
///
/// `>` for an `ASC` field, `<` for `DESC`. Placeholders start at
/// `param_offset` and run in ordering order.
///
/// Returns `(None, vec![])` for an empty cursor — the first page emits
/// nothing, binds nothing, and is not an error.
///
/// # Why the value list depends on the dialect
///
/// Every clause but the first pins the more-significant columns to values an
/// earlier clause already referenced. A numbered dialect says so — `$1` appears
/// in each of them and is bound once — but a positional `?` has no way to point
/// backwards, so each one consumes its own bind and the value list repeats.
/// Three ordering fields bind three values on Postgres and six on SQLite, for
/// the same predicate.
///
/// Getting this wrong does not raise an error. The binds shift by one and the
/// page silently resumes from the wrong row.
pub(crate) fn rewrite(
    order_by: &OrderBy,
    cursor: &[CursorValue],
    columns: Columns<'_>,
    dialect: &impl Dialect,
    param_offset: usize,
) -> Result<(Option<String>, Vec<Value>), Error> {
    if cursor.is_empty() {
        return Ok((None, Vec::new()));
    }
    let fields = &order_by.fields;
    if fields.len() != cursor.len() {
        return Err(Error::CursorArity {
            fields: fields.len(),
            values: cursor.len(),
        });
    }

    let quoted: Vec<String> = fields
        .iter()
        .map(|field| {
            let column = column(columns, &field.path, Dimension::Cursor)?;
            Ok(dialect.quote_ident(column))
        })
        .collect::<Result<_, Error>>()?;
    let keys: Vec<Value> = fields
        .iter()
        .zip(cursor)
        .map(|(field, value)| bind(field, value))
        .collect::<Result<_, Error>>()?;

    let positional = is_positional(dialect);
    let mut repeated: Vec<Value> = Vec::new();

    let mut sql = String::from("(");
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            sql.push_str(" OR ");
        }
        sql.push('(');
        for term in 0..=index {
            if term > 0 {
                sql.push_str(" AND ");
            }
            // Either way this is the true ordinal of the parameter being
            // referenced -- the difference is only whether an earlier bind can
            // be named again, or has to be repeated. A positional dialect
            // ignores the number, but it is still the honest answer.
            let slot = if positional {
                repeated.push(keys[term].clone());
                param_offset + repeated.len() - 1
            } else {
                param_offset + term
            };
            // Every term but the last pins a more-significant column to its
            // cursor value, so this clause only decides ties the earlier ones
            // left open.
            let operator = if term < index {
                "="
            } else if field.desc {
                "<"
            } else {
                ">"
            };
            write!(
                sql,
                "{} {operator} {}",
                quoted[term],
                dialect.placeholder(slot)
            )
            .expect("a String cannot fail");
        }
        sql.push(')');
    }
    sql.push(')');

    Ok((Some(sql), if positional { repeated } else { keys }))
}

/// Whether `dialect` renders every placeholder alike, so that a bind cannot be
/// referenced twice.
///
/// Decided by asking the dialect rather than by naming the three built in, so a
/// caller's own [`Dialect`] is classified correctly too. It handles the awkward
/// middle case for free: a dialect emitting SQLite's numbered `?1` / `?2` form
/// is *positional in syntax but addressable*, renders the two differently, and
/// is correctly treated as numbered.
///
/// This infers a behavioural property from rendered text, which is a smell. The
/// honest fix is a `Dialect::is_positional` in sqlx-cel, defaulting to exactly
/// this comparison; until that exists, this is the only signal the trait
/// offers.
fn is_positional(dialect: &impl Dialect) -> bool {
    // Two arbitrary adjacent indices. Any dialect that distinguishes parameters
    // at all distinguishes these, so rendering them alike means it does not.
    dialect.placeholder(1) == dialect.placeholder(2)
}

/// Converts one cursor value into the bind value it compares against.
///
/// Mechanical apart from the null: [`CursorValue`] widens sized integers to 64
/// bits and `f32` to `f64` on decode, so every other variant has a
/// [`Value`] waiting for it.
///
/// The match is exhaustive on purpose. A new [`CursorValue`] variant should
/// break this build rather than fall into a `_` arm that guesses.
fn bind(field: &OrderByField, value: &CursorValue) -> Result<Value, Error> {
    Ok(match value {
        // Not bound, deliberately. `col > $1` with a NULL bind evaluates to
        // NULL rather than true, so the row is dropped and pagination stops
        // early with no error anywhere. See the crate docs.
        CursorValue::Null => {
            return Err(Error::NullCursorValue {
                path: field.path.clone(),
            });
        }
        CursorValue::Bool(value) => Value::Bool(*value),
        CursorValue::String(value) => Value::Text(value.clone()),
        CursorValue::Bytes(value) => Value::Bytes(value.clone()),
        CursorValue::Int(value) => Value::Int(*value),
        CursorValue::Uint(value) => Value::Uint(*value),
        CursorValue::Float(value) => Value::Float(*value),
        CursorValue::Timestamp { seconds, nanos } => timestamp(field, *seconds, *nanos)?,
        CursorValue::Duration { nanos } => Value::Duration(microseconds(*nanos)),
    })
}

/// Rounds a signed nanosecond count to microseconds, half away from zero.
///
/// A Postgres interval has microsecond resolution, so the sub-microsecond part
/// of a `google.protobuf.Duration` has nowhere to go. Rounding rather than
/// truncating matches how sqlx-cel parses a `duration("…")` literal, which is
/// what the cursor value is compared against.
fn microseconds(nanos: i64) -> i64 {
    // Widened first: `i64::MAX + 500` overflows, and the quotient always fits
    // back into an `i64` because it is a thousandth of one.
    let nanos = i128::from(nanos);
    let micros = if nanos >= 0 {
        (nanos + 500) / 1_000
    } else {
        (nanos - 500) / 1_000
    };
    i64::try_from(micros).expect("a thousandth of an i64 fits in an i64")
}

/// Builds the `chrono` instant a cursor timestamp compares against.
#[cfg(feature = "chrono")]
fn timestamp(field: &OrderByField, seconds: i64, nanos: i32) -> Result<Value, Error> {
    let out_of_range = || Error::CursorTimestamp {
        path: field.path.clone(),
        seconds,
    };
    // `aip-rs` normalizes `nanos` into `0..1_000_000_000` when it encodes, but
    // a page token is client-supplied and decoding does not re-check, so a
    // crafted one can carry a negative.
    let nanos = u32::try_from(nanos).map_err(|_| out_of_range())?;
    chrono::DateTime::from_timestamp(seconds, nanos)
        .map(Value::Timestamp)
        .ok_or_else(out_of_range)
}

/// Builds the `time` instant a cursor timestamp compares against.
///
/// The `cfg` mirrors sqlx-cel's: with both features on, `chrono` wins.
#[cfg(all(feature = "time", not(feature = "chrono")))]
fn timestamp(field: &OrderByField, seconds: i64, nanos: i32) -> Result<Value, Error> {
    let out_of_range = || Error::CursorTimestamp {
        path: field.path.clone(),
        seconds,
    };
    let nanos = u32::try_from(nanos).map_err(|_| out_of_range())?;
    let since_epoch = i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    time::OffsetDateTime::from_unix_timestamp_nanos(since_epoch)
        .map(Value::Timestamp)
        .map_err(|_| out_of_range())
}

#[cfg(test)]
mod tests {
    use super::{microseconds, rewrite};
    use crate::error::{Dimension, Error};
    use aip::{CursorValue, OrderBy};
    use sqlx_cel::dialect::{Dialect, MySql, Postgres, Sqlite};
    use sqlx_cel::{Columns, Value};

    /// SQLite's *other* placeholder form. `?1` is positional in syntax but
    /// still addressable, so a bind can be referenced twice and the values must
    /// not repeat -- the case a "does it look like `?`" test would get wrong.
    #[derive(Clone, Copy)]
    struct NumberedSqlite;

    impl Dialect for NumberedSqlite {
        fn name(&self) -> &'static str {
            "sqlite-numbered"
        }
        fn placeholder(&self, index: usize) -> String {
            format!("?{index}")
        }
        fn regex(&self, lhs: &str, rhs: &str) -> Option<String> {
            Some(format!("{lhs} REGEXP {rhs}"))
        }
    }

    const COLUMNS: Columns<'static> = Columns::new(&[
        ("title", "volumes.title"),
        ("create_time", "volumes.created_at"),
        ("id", "volumes.id"),
    ]);

    fn order_by(spec: &str) -> OrderBy {
        spec.parse().unwrap()
    }

    /// The one-field case keeps both sets of parentheses -- the per-clause pair
    /// and the wrapper around the `OR` chain. Redundant here, but the wrapper
    /// is what lets `rewrite` `AND` a filter onto the predicate without
    /// inspecting its shape.
    #[test]
    fn a_single_ascending_field_is_one_comparison() {
        let (sql, values) = rewrite(
            &order_by("id"),
            &[CursorValue::Int(7)],
            COLUMNS,
            &Postgres,
            1,
        )
        .unwrap();
        assert_eq!(sql.as_deref(), Some(r#"(("volumes"."id" > $1))"#));
        assert_eq!(values, vec![Value::Int(7)]);
    }

    /// The shape the whole module exists for: an equality prefix per clause,
    /// and an operator that follows each field's own direction.
    #[test]
    fn each_field_carries_its_own_direction() {
        let (sql, values) = rewrite(
            &order_by("title, create_time desc, id"),
            &[
                CursorValue::String("Dune".to_owned()),
                CursorValue::timestamp(1_700_000_000, 0),
                CursorValue::Int(7),
            ],
            COLUMNS,
            &Postgres,
            1,
        )
        .unwrap();
        assert_eq!(
            sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."title" > $1)"#,
                r#" OR ("volumes"."title" = $1 AND "volumes"."created_at" < $2)"#,
                r#" OR ("volumes"."title" = $1 AND "volumes"."created_at" = $2"#,
                r#" AND "volumes"."id" > $3))"#,
            )),
        );
        assert_eq!(values.len(), 3);
        assert_eq!(values[0], Value::Text("Dune".to_owned()));
    }

    /// A positional dialect cannot point back at an earlier bind, so every `?`
    /// gets its own -- three fields bind six values, not three. Off-by-one here
    /// resumes the page from the wrong row rather than raising anything.
    #[test]
    fn a_positional_dialect_repeats_the_values_its_placeholders_cannot_share() {
        let cursor = [CursorValue::String("Dune".to_owned()), CursorValue::Int(7)];
        let (sql, values) = rewrite(&order_by("title, id"), &cursor, COLUMNS, &Sqlite, 1).unwrap();
        assert_eq!(
            sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."title" > ?)"#,
                r#" OR ("volumes"."title" = ? AND "volumes"."id" > ?))"#,
            )),
        );
        // Three placeholders, so three values -- the title twice, in the order
        // the `?`s consume them.
        assert_eq!(
            values,
            vec![
                Value::Text("Dune".to_owned()),
                Value::Text("Dune".to_owned()),
                Value::Int(7),
            ],
        );

        // MySQL is positional too, and differs only in how it quotes.
        let (sql, mysql_values) =
            rewrite(&order_by("title, id"), &cursor, COLUMNS, &MySql, 1).unwrap();
        assert_eq!(
            sql.as_deref(),
            Some("((`volumes`.`title` > ?) OR (`volumes`.`title` = ? AND `volumes`.`id` > ?))"),
        );
        assert_eq!(mysql_values, values);
    }

    /// A dialect can be positional in syntax and still addressable. Deciding by
    /// asking it to render two indices catches that; deciding by whether the
    /// placeholder contains a `?` would not.
    #[test]
    fn a_numbered_dialect_is_not_treated_as_positional_just_for_using_a_question_mark() {
        let (sql, values) = rewrite(
            &order_by("title, id"),
            &[CursorValue::String("Dune".to_owned()), CursorValue::Int(7)],
            COLUMNS,
            &NumberedSqlite,
            1,
        )
        .unwrap();
        assert_eq!(
            sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."title" > ?1)"#,
                r#" OR ("volumes"."title" = ?1 AND "volumes"."id" > ?2))"#,
            )),
        );
        // Two, not three: `?1` names the same bind in both clauses.
        assert_eq!(values, vec![Value::Text("Dune".to_owned()), Value::Int(7)],);
    }

    /// The filter's literals are bound first, so the cursor's placeholders
    /// start after them.
    #[test]
    fn placeholders_start_at_the_offset() {
        let (sql, _) = rewrite(
            &order_by("title, id"),
            &[CursorValue::String("Dune".to_owned()), CursorValue::Int(7)],
            COLUMNS,
            &Postgres,
            4,
        )
        .unwrap();
        assert_eq!(
            sql.as_deref(),
            Some(concat!(
                r#"(("volumes"."title" > $4)"#,
                r#" OR ("volumes"."title" = $4 AND "volumes"."id" > $5))"#,
            )),
        );
    }

    #[test]
    fn an_empty_cursor_is_the_first_page() {
        assert_eq!(
            rewrite(&order_by("title"), &[], COLUMNS, &Postgres, 1).unwrap(),
            (None, Vec::new()),
        );
        // Even with no ordering at all: nothing to resume from is not a
        // mismatch.
        assert_eq!(
            rewrite(&OrderBy::default(), &[], COLUMNS, &Postgres, 1).unwrap(),
            (None, Vec::new()),
        );
    }

    #[test]
    fn a_cursor_of_the_wrong_length_is_rejected() {
        for (spec, cursor, fields, values) in [
            ("title, id", vec![CursorValue::Int(7)], 2, 1),
            (
                "title",
                vec![CursorValue::Int(7), CursorValue::Int(8)],
                1,
                2,
            ),
            ("", vec![CursorValue::Int(7)], 0, 1),
        ] {
            assert_eq!(
                rewrite(&order_by(spec), &cursor, COLUMNS, &Postgres, 1).unwrap_err(),
                Error::CursorArity { fields, values },
                "for order_by {spec:?}",
            );
        }
    }

    #[test]
    fn a_null_cursor_value_is_rejected_rather_than_bound() {
        assert_eq!(
            rewrite(
                &order_by("title, id"),
                &[CursorValue::String("Dune".to_owned()), CursorValue::Null],
                COLUMNS,
                &Postgres,
                1,
            )
            .unwrap_err(),
            Error::NullCursorValue {
                path: "id".to_owned()
            },
        );
    }

    #[test]
    fn a_path_outside_the_column_map_fails_as_the_cursor_dimension() {
        assert_eq!(
            rewrite(
                &order_by("shoe_size"),
                &[CursorValue::Int(7)],
                COLUMNS,
                &Postgres,
                1,
            )
            .unwrap_err(),
            Error::UnknownField {
                dimension: Dimension::Cursor,
                path: "shoe_size".to_owned(),
            },
        );
    }

    /// Signed, and rounded the way sqlx-cel rounds a `duration("…")` literal.
    #[test]
    fn a_duration_narrows_to_signed_microseconds() {
        assert_eq!(microseconds(1_500), 2);
        assert_eq!(microseconds(-1_500), -2);
        assert_eq!(microseconds(1_499), 1);
        assert_eq!(microseconds(-1_499), -1);
        assert_eq!(microseconds(0), 0);
        assert_eq!(microseconds(-90 * 60 * 1_000_000_000), -90 * 60 * 1_000_000);
        // The widening is what keeps the extremes from overflowing.
        assert_eq!(microseconds(i64::MAX), (i64::MAX / 1_000) + 1);
        assert_eq!(microseconds(i64::MIN), (i64::MIN / 1_000) - 1);
    }

    #[test]
    fn every_non_null_variant_maps_to_a_value() {
        let cursor = vec![
            CursorValue::Bool(true),
            CursorValue::String("Dune".to_owned()),
            CursorValue::Bytes(vec![1, 2, 3]),
            CursorValue::Int(-7),
            CursorValue::Uint(7),
            CursorValue::Float(1.5),
            CursorValue::timestamp(1_700_000_000, 500),
            CursorValue::duration_nanos(-1_500),
        ];
        let columns = Columns::new(&[
            ("a", "a"),
            ("b", "b"),
            ("c", "c"),
            ("d", "d"),
            ("e", "e"),
            ("f", "f"),
            ("g", "g"),
            ("h", "h"),
        ]);
        let (_, values) = rewrite(
            &order_by("a, b, c, d, e, f, g, h"),
            &cursor,
            columns,
            &Postgres,
            1,
        )
        .unwrap();

        assert_eq!(values[0], Value::Bool(true));
        assert_eq!(values[1], Value::Text("Dune".to_owned()));
        assert_eq!(values[2], Value::Bytes(vec![1, 2, 3]));
        assert_eq!(values[3], Value::Int(-7));
        assert_eq!(values[4], Value::Uint(7));
        assert_eq!(values[5], Value::Float(1.5));
        assert!(matches!(values[6], Value::Timestamp(_)));
        assert_eq!(values[7], Value::Duration(-2));
    }

    /// Page tokens are client-supplied and their checksum is computed from the
    /// request alone, so an absurd timestamp is reachable and must not panic.
    #[test]
    fn a_timestamp_outside_the_backend_range_is_an_error() {
        let error = rewrite(
            &order_by("create_time"),
            &[CursorValue::Timestamp {
                seconds: i64::MAX,
                nanos: 0,
            }],
            COLUMNS,
            &Postgres,
            1,
        )
        .unwrap_err();
        assert_eq!(
            error,
            Error::CursorTimestamp {
                path: "create_time".to_owned(),
                seconds: i64::MAX,
            },
        );
    }
}
