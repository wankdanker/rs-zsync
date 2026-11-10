//! Text tables (node-zsync used `text-table`) and the JSON envelope.

use crate::backend::Envelope;
use serde::Serialize;

/// Left-aligned columns separated by two spaces, trailing whitespace trimmed.
pub fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    rows.iter()
        .map(|r| {
            let line: Vec<String> = r.iter().enumerate().map(|(i, cell)| format!("{cell:<w$}", w = widths[i])).collect();
            line.join("  ").trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn json_ok<T: Serialize>(value: &T) -> String {
    serde_json::to_string(&Envelope::ok(serde_json::to_value(value).unwrap_or(serde_json::Value::Null))).expect("serializable")
}

pub fn json_err(message: &str) -> String {
    serde_json::to_string(&Envelope::err(message.into())).expect("serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_columns() {
        let rows = vec![
            vec!["source".into(), "destination".into(), "up-to-date".into()],
            vec!["pool1/sub1".into(), "pool3/sub1".into(), "false".into()],
        ];
        assert_eq!(table(&rows), "source      destination  up-to-date\npool1/sub1  pool3/sub1   false");
    }

    #[test]
    fn trailing_empty_cells_are_trimmed() {
        let rows = vec![vec!["a".into(), "".into()], vec!["bbb".into(), "".into()]];
        assert_eq!(table(&rows), "a\nbbb");
    }
}
