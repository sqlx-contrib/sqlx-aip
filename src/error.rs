use core::fmt;

/// Which of a `List` request's three query dimensions a path came from.
///
/// Carried by [`Error::UnknownField`] so the message can name it. It is
/// deliberately *not* what distinguishes one error variant from another: an
/// unmapped path means the same thing about the column map wherever it turns
/// up, and mapping it to an RPC status is the same judgement call in all three
/// cases. See [`Error`].
///
/// Deliberately *not* `#[non_exhaustive]`: an AIP `List` request has exactly
/// these three query dimensions, and the crate's scope excludes taking on more,
/// so a caller mapping them to RPC statuses should get a compile error if that
/// ever stops being true rather than a `_` arm that silently absorbs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// The AIP-160 `filter` expression.
    Filter,
    /// The AIP-132 `order_by` list.
    OrderBy,
    /// The key-set cursor carried by the AIP-158 `page_token`.
    Cursor,
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Filter => "filter",
            Self::OrderBy => "order_by",
            Self::Cursor => "page_token",
        })
    }
}

/// Everything [`Query::rewrite`](crate::Query::rewrite) can fail with.
///
/// The variants are worth distinguishing because each says something different
/// about the caller:
///
/// - [`UnknownField`](Self::UnknownField) — a configuration bug in the column
///   map, or a client probing a field the proto declares but the map
///   withholds. The crate cannot tell those apart, so it does not pretend to:
///   this is the one variant that is arguably `Internal` rather than
///   `InvalidArgument` at the RPC boundary, and only the caller knows which.
/// - [`CursorArity`](Self::CursorArity) — a token issued under a different
///   ordering.
/// - [`NullCursorValue`](Self::NullCursorValue) — an ordering column that is
///   nullable. See the crate docs.
/// - [`CursorTimestamp`](Self::CursorTimestamp) — a cursor timestamp outside
///   the range the active date-time backend can represent.
/// - [`Filter`](Self::Filter) — anything sqlx-cel rejected, with the source
///   preserved.
///
/// Every variant but the first maps to `InvalidArgument`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A path that is not in the column map.
    ///
    /// This is the fail-closed gate, and it applies to all three dimensions:
    /// the CEL environment and the `order_by` allow-list both declare more
    /// fields than the column map is obliged to expose.
    UnknownField {
        /// Where the path came from.
        dimension: Dimension,
        /// The dotted AIP path that was not found.
        path: String,
    },

    /// A cursor whose length does not match the number of ordering fields.
    ///
    /// The i'th cursor value is the i'th ordering field's value on the last row
    /// of the previous page, so a mismatch means the token was issued under a
    /// different `order_by`. Truncating to the shorter of the two would resume
    /// at the wrong place rather than fail.
    CursorArity {
        /// How many ordering fields there are.
        fields: usize,
        /// How many values the cursor carries.
        values: usize,
    },

    /// A [`CursorValue::Null`](aip::CursorValue::Null) in the cursor.
    ///
    /// Rejected rather than bound, because `col > $1` with a NULL bind
    /// evaluates to NULL and silently drops the row. See the crate docs.
    NullCursorValue {
        /// The ordering path whose cursor value was null.
        path: String,
    },

    /// A cursor timestamp the active date-time backend cannot represent.
    ///
    /// `google.protobuf.Timestamp` spans a wider range of seconds than either
    /// `chrono` or `time` does, and page tokens are client-supplied, so a
    /// crafted one can carry a value that has nowhere to go.
    CursorTimestamp {
        /// The ordering path whose cursor value was out of range.
        path: String,
        /// The seconds since the Unix epoch that could not be represented.
        seconds: i64,
    },

    /// The filter expression was rejected by sqlx-cel.
    ///
    /// [`Error::UnknownField`] is lifted out of this variant, so that an
    /// unmapped path is one thing to match on whichever dimension produced it.
    /// Everything else — an unsupported function, a macro, a malformed
    /// `timestamp()` literal — arrives here with its source intact.
    Filter(sqlx_cel::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownField { dimension, path } => {
                write!(f, "{dimension}: unknown field {path:?}")
            }
            Self::CursorArity { fields, values } => write!(
                f,
                "page_token: cursor carries {values} value{} for {fields} ordering field{}; \
                 it was issued under a different order_by",
                if *values == 1 { "" } else { "s" },
                if *fields == 1 { "" } else { "s" },
            ),
            Self::NullCursorValue { path } => write!(
                f,
                "page_token: ordering field {path:?} is null in the cursor; \
                 an ordering column must be NOT NULL",
            ),
            Self::CursorTimestamp { path, seconds } => write!(
                f,
                "page_token: ordering field {path:?}: the timestamp at {seconds}s \
                 is outside the representable range",
            ),
            Self::Filter(error) => write!(f, "filter: {error}"),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Filter(error) => Some(error),
            _ => None,
        }
    }
}

impl From<sqlx_cel::Error> for Error {
    /// Lifts an unmapped path out into [`Error::UnknownField`], and wraps
    /// everything else.
    fn from(error: sqlx_cel::Error) -> Self {
        match error {
            sqlx_cel::Error::UnknownField { path } => Self::UnknownField {
                dimension: Dimension::Filter,
                path,
            },
            error => Self::Filter(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Dimension, Error};

    #[test]
    fn an_unmapped_path_names_the_dimension_it_came_from() {
        for (dimension, expected) in [
            (Dimension::Filter, r#"filter: unknown field "notes""#),
            (Dimension::OrderBy, r#"order_by: unknown field "notes""#),
            (Dimension::Cursor, r#"page_token: unknown field "notes""#),
        ] {
            let error = Error::UnknownField {
                dimension,
                path: "notes".to_owned(),
            };
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn an_arity_mismatch_reports_both_counts() {
        assert_eq!(
            Error::CursorArity {
                fields: 2,
                values: 1
            }
            .to_string(),
            "page_token: cursor carries 1 value for 2 ordering fields; \
             it was issued under a different order_by",
        );
    }

    #[test]
    fn a_null_cursor_value_points_at_the_fix() {
        let error = Error::NullCursorValue {
            path: "create_time".to_owned(),
        };
        assert!(error.to_string().contains("NOT NULL"), "{error}");
        assert!(error.to_string().contains("create_time"), "{error}");
    }

    /// The filter's unmapped path is the same variant the ordering's is, so a
    /// caller matching on it does not have to know which dimension failed.
    #[test]
    fn an_unmapped_filter_path_is_lifted_out_of_the_wrapper() {
        let error = Error::from(sqlx_cel::Error::UnknownField {
            path: "notes".to_owned(),
        });
        assert_eq!(
            error,
            Error::UnknownField {
                dimension: Dimension::Filter,
                path: "notes".to_owned(),
            },
        );
        assert!(core::error::Error::source(&error).is_none());
    }

    #[test]
    fn anything_else_from_sqlx_cel_keeps_its_source() {
        let error = Error::from(sqlx_cel::Error::NotAList);
        assert_eq!(
            error.to_string(),
            format!("filter: {}", sqlx_cel::Error::NotAList)
        );
        assert!(core::error::Error::source(&error).is_some());
    }
}
