//! 对应 `src/storage/scan.ts`：内置存储扫描的起点与续扫游标。
//!
//! 游标**延续其被创建时的顺序**：查询重复该顺序或省略 `order` 都继续该顺序；要求另一种顺序则报错。
//! 没有 `order` 字段的旧游标（在扫描支持顺序之前写入的）按扫描的默认顺序继续。

use crate::types::{Cursor, Page, ScanOrder};

/// 对应 `ScanStart`：有序扫描的起点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanStart {
    /// 扫描顺序。
    pub order: ScanOrder,
    /// 上一页返回的最后一个 ID。
    pub after: Option<u64>,
}

/// 对应 `scanStart` 抛出的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// `order` 不是合法取值。
    InvalidOrder(String),
    /// 游标结构非法。
    InvalidCursor,
    /// 游标延续的顺序与查询要求的顺序不同。
    OrderMismatch {
        /// 游标携带的顺序。
        cursor: ScanOrder,
        /// 查询要求的顺序。
        requested: ScanOrder,
    },
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::InvalidOrder(value) => write!(f, "Invalid scan order: {value}"),
            ScanError::InvalidCursor => write!(f, "Invalid storage cursor"),
            ScanError::OrderMismatch { cursor, requested } => write!(
                f,
                "The cursor continues a {} scan; the query asks for {}",
                order_name(*cursor),
                order_name(*requested),
            ),
        }
    }
}

impl std::error::Error for ScanError {}

/// 对应 `scanStart`。
pub fn scan_start(
    requested: Option<ScanOrder>,
    cursor: Option<&Cursor>,
    fallback: ScanOrder,
) -> Result<ScanStart, ScanError> {
    let Some(cursor) = cursor else {
        return Ok(ScanStart {
            order: requested.unwrap_or(fallback),
            after: None,
        });
    };

    let after = cursor
        .get("after")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ScanError::InvalidCursor)?;

    let stored = match cursor.get("order") {
        None => None,
        Some(value) => Some(parse_order(value).ok_or(ScanError::InvalidCursor)?),
    };

    let order = stored.unwrap_or(fallback);
    if let Some(requested) = requested
        && requested != order
    {
        return Err(ScanError::OrderMismatch {
            cursor: order,
            requested,
        });
    }

    Ok(ScanStart {
        order,
        after: Some(after),
    })
}

/// 对应 `nextCursor`：以最后一个返回项的 `id` 作为续扫点。
pub fn next_cursor(id: u64, order: ScanOrder) -> Cursor {
    let mut cursor = Cursor::new();
    cursor.insert("after".to_string(), serde_json::Value::from(id));
    cursor.insert(
        "order".to_string(),
        serde_json::Value::String(order_name(order).to_string()),
    );
    cursor
}

/// 对应 `page`：截断到 `limit`，必要时给出续扫游标。
///
/// 各后端共享同一分页语义（memory / sqlite / jsonl）。
pub fn page<T: Clone>(
    values: Vec<T>,
    limit: usize,
    order: ScanOrder,
    id_of: impl Fn(&T) -> u64,
) -> Page<T, Cursor> {
    if values.len() <= limit {
        return Page {
            items: values,
            next: None,
        };
    }
    let mut items = values;
    let next = items
        .get(limit.saturating_sub(1))
        .map(|last| next_cursor(id_of(last), order));
    items.truncate(limit);
    Page { items, next }
}

fn order_name(order: ScanOrder) -> &'static str {
    match order {
        ScanOrder::Ascending => "ascending",
        ScanOrder::Descending => "descending",
    }
}

fn parse_order(value: &serde_json::Value) -> Option<ScanOrder> {
    match value.as_str()? {
        "ascending" => Some(ScanOrder::Ascending),
        "descending" => Some(ScanOrder::Descending),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cursor(after: serde_json::Value, order: Option<&str>) -> Cursor {
        let mut cursor = Cursor::new();
        cursor.insert("after".to_string(), after);
        if let Some(order) = order {
            cursor.insert("order".to_string(), json!(order));
        }
        cursor
    }

    #[test]
    fn no_cursor_uses_requested_or_fallback() {
        assert_eq!(
            scan_start(None, None, ScanOrder::Ascending).unwrap(),
            ScanStart {
                order: ScanOrder::Ascending,
                after: None
            },
        );
        assert_eq!(
            scan_start(Some(ScanOrder::Descending), None, ScanOrder::Ascending)
                .unwrap()
                .order,
            ScanOrder::Descending,
        );
    }

    #[test]
    fn cursor_continues_its_own_order_when_query_omits_order() {
        let cursor = cursor(json!(5), Some("descending"));
        let start = scan_start(None, Some(&cursor), ScanOrder::Ascending).unwrap();
        assert_eq!(start.order, ScanOrder::Descending, "游标携带的顺序优先");
        assert_eq!(start.after, Some(5));
    }

    #[test]
    fn legacy_cursor_without_order_falls_back() {
        let cursor = cursor(json!(3), None);
        let start = scan_start(None, Some(&cursor), ScanOrder::Descending).unwrap();
        assert_eq!(start.order, ScanOrder::Descending);
        assert_eq!(start.after, Some(3));
    }

    #[test]
    fn mismatched_requested_order_is_rejected() {
        let cursor = cursor(json!(1), Some("ascending"));
        let error = scan_start(
            Some(ScanOrder::Descending),
            Some(&cursor),
            ScanOrder::Ascending,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "The cursor continues a ascending scan; the query asks for descending",
        );
    }

    #[test]
    fn malformed_cursor_is_rejected() {
        let missing_after = {
            let mut cursor = Cursor::new();
            cursor.insert("order".to_string(), json!("ascending"));
            cursor
        };
        assert_eq!(
            scan_start(None, Some(&missing_after), ScanOrder::Ascending).unwrap_err(),
            ScanError::InvalidCursor,
        );

        let bad_order = cursor(json!(1), Some("sideways"));
        assert_eq!(
            scan_start(None, Some(&bad_order), ScanOrder::Ascending).unwrap_err(),
            ScanError::InvalidCursor,
        );
    }

    #[test]
    fn next_cursor_records_id_and_order() {
        let cursor = next_cursor(9, ScanOrder::Descending);
        assert_eq!(cursor.get("after"), Some(&json!(9)));
        assert_eq!(cursor.get("order"), Some(&json!("descending")));
        // 往返：生成的游标能被 scan_start 解析。
        let start = scan_start(None, Some(&cursor), ScanOrder::Ascending).unwrap();
        assert_eq!(start.order, ScanOrder::Descending);
        assert_eq!(start.after, Some(9));
    }
}
