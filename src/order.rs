use aip::OrderBy;
use sqlx_cel::Columns;
use sqlx_cel::dialect::{Dialect, Postgres};

use crate::column;
use crate::error::{Dimension, Error};

/// Rewrites an AIP-132 ordering into a comma-separated list of
/// `"col" ASC|DESC` terms, without the `ORDER BY` prefix.
///
/// Returns `None` when there are no fields. That is the server's choice of
/// order, not an error.
///
/// No `NULLS FIRST` / `NULLS LAST` is emitted. Postgres sorts NULLs last for
/// `ASC` and first for `DESC`, and the key-set predicate in
/// [`crate::cursor`] rejects a null cursor value outright, so the two agree by
/// construction. Emitting one without the other would not.
pub(crate) fn rewrite(order_by: &OrderBy, columns: Columns<'_>) -> Result<Option<String>, Error> {
    if order_by.is_empty() {
        return Ok(None);
    }
    let mut sql = String::new();
    for (index, field) in order_by.fields.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        let column = column(columns, &field.path, Dimension::OrderBy)?;
        sql.push_str(&Postgres.quote_ident(column));
        sql.push_str(if field.desc { " DESC" } else { " ASC" });
    }
    Ok(Some(sql))
}

#[cfg(test)]
mod tests {
    use super::rewrite;
    use crate::error::{Dimension, Error};
    use aip::OrderBy;
    use sqlx_cel::Columns;

    const COLUMNS: Columns<'static> = Columns::new(&[
        ("title", "volumes.title"),
        ("create_time", "volumes.created_at"),
        ("rank", "rank"),
    ]);

    fn sql(order_by: &str) -> Option<String> {
        rewrite(&order_by.parse::<OrderBy>().unwrap(), COLUMNS).unwrap()
    }

    #[test]
    fn maps_paths_to_columns_and_directions() {
        assert_eq!(
            sql("title, create_time desc").as_deref(),
            Some(r#""volumes"."title" ASC, "volumes"."created_at" DESC"#),
        );
    }

    /// `asc` is the default and is emitted explicitly, so the term reads the
    /// same whether or not the client wrote it.
    #[test]
    fn an_unwritten_direction_is_still_ascending() {
        assert_eq!(sql("rank").as_deref(), Some(r#""rank" ASC"#));
        assert_eq!(sql("rank asc").as_deref(), Some(r#""rank" ASC"#));
    }

    /// The empty ordering is the server's choice, not an error -- the caller
    /// omits the `ORDER BY` keyword.
    #[test]
    fn no_fields_is_no_clause() {
        assert_eq!(sql(""), None);
    }

    #[test]
    fn a_path_outside_the_column_map_fails() {
        let order_by = "shoe_size".parse::<OrderBy>().unwrap();
        assert_eq!(
            rewrite(&order_by, COLUMNS).unwrap_err(),
            Error::UnknownField {
                dimension: Dimension::OrderBy,
                path: "shoe_size".to_owned(),
            },
        );
    }
}
