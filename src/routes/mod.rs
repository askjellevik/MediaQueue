pub mod admin;
pub mod internal;
pub mod public;
pub mod static_files;

use crate::error::AppError;

const DEFAULT_PAGE: i64 = 20;
const MAX_PAGE: i64 = 50;

/// Parses a `created_at.id` keyset cursor. No cursor (the first page)
/// starts past every row.
pub fn parse_cursor(cursor: Option<&str>) -> Result<(i64, i64), AppError> {
    let Some(cursor) = cursor.filter(|c| !c.is_empty()) else {
        return Ok((i64::MAX, i64::MAX));
    };
    let (created_at, id) = cursor.split_once('.').ok_or(AppError::BadRequest)?;
    let created_at = created_at.parse().map_err(|_| AppError::BadRequest)?;
    let id = id.parse().map_err(|_| AppError::BadRequest)?;
    Ok((created_at, id))
}

pub fn format_cursor(created_at: i64, id: i64) -> String {
    format!("{created_at}.{id}")
}

/// Page size: 20 by default, 1 to 50 when given.
pub fn page_limit(limit: Option<i64>) -> Result<i64, AppError> {
    match limit {
        None => Ok(DEFAULT_PAGE),
        Some(n) if (1..=MAX_PAGE).contains(&n) => Ok(n),
        Some(_) => Err(AppError::BadRequest),
    }
}

/// Fetching `limit + 1` rows tells whether another page exists; this
/// trims the extra row and returns the cursor for the next page.
pub fn next_page<T>(
    rows: &mut Vec<T>,
    limit: i64,
    key: impl Fn(&T) -> (i64, i64),
) -> Option<String> {
    let limit = usize::try_from(limit).unwrap_or(0);
    if rows.len() <= limit {
        return None;
    }
    rows.truncate(limit);
    rows.last().map(|r| {
        let (created_at, id) = key(r);
        format_cursor(created_at, id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        assert_eq!(parse_cursor(None).unwrap(), (i64::MAX, i64::MAX));
        assert_eq!(parse_cursor(Some(&format_cursor(5, 7))).unwrap(), (5, 7));
        assert!(parse_cursor(Some("5")).is_err());
        assert!(parse_cursor(Some("a.b")).is_err());
        assert_eq!(page_limit(None).unwrap(), DEFAULT_PAGE);
        assert!(page_limit(Some(0)).is_err());
        assert!(page_limit(Some(51)).is_err());
    }

    #[test]
    fn next_page_trims_the_probe_row() {
        let mut rows = vec![(9, 3), (8, 2), (7, 1)];
        assert_eq!(next_page(&mut rows, 2, |r| *r).as_deref(), Some("8.2"));
        assert_eq!(rows.len(), 2);
        let mut last = vec![(1, 1)];
        assert_eq!(next_page(&mut last, 2, |r| *r), None);
    }
}
