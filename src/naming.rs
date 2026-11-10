//! Dataset and snapshot naming: destination mapping (`nameDrop`), zsync snapshot names and the
//! tag patterns rotate/prune select on.

use regex::Regex;

/// Map a source dataset name to the part appended to the destination base.
///
/// Mirrors node-zsync's `nameDrop(name, drop, keep)`:
/// * `drop == 0 && keep == 0` (both given as zero) keeps the full name;
/// * neither given (or both zero-ish without being explicit) keeps the last element;
/// * otherwise drop `drop` elements from the left, then keep `keep` elements from the right.
pub fn name_drop(name: &str, drop: Option<usize>, keep: Option<usize>) -> String {
    if drop == Some(0) && keep == Some(0) {
        return name.to_string();
    }
    let drop = drop.unwrap_or(0);
    let mut keep = keep.unwrap_or(0);
    if drop == 0 && keep == 0 {
        keep = 1;
    }
    let mut parts: Vec<&str> = name.split('/').collect();
    if drop > 0 {
        parts.drain(0..drop.min(parts.len()));
    }
    if keep > 0 && keep < parts.len() {
        parts.drain(0..parts.len() - keep);
    }
    parts.join("/")
}

/// `path.join(destination, name)` for dataset names.
pub fn dest_join(base: &str, name: &str) -> String {
    let base = base.trim_end_matches('/');
    let name = name.trim_matches('/');
    match (base.is_empty(), name.is_empty()) {
        (true, _) => name.to_string(),
        (_, true) => base.to_string(),
        _ => format!("{base}/{name}"),
    }
}

/// The destination dataset for `source` under `base`.
pub fn destination_for(source: &str, base: &str, drop: Option<usize>, keep: Option<usize>) -> String {
    dest_join(base, &name_drop(source, drop, keep))
}

/// `prefix[-tag]-timestamp`, e.g. `zsync-hourly-20240101120000000`.
pub fn snapshot_name(prefix: &str, tag: Option<&str>, timestamp: &str) -> String {
    let mut parts = vec![prefix];
    if let Some(t) = tag.filter(|t| !t.is_empty()) {
        parts.push(t);
    }
    parts.push(timestamp);
    parts.join("-")
}

/// The regex rotate uses to select snapshots: `^prefix[-tag]-[0-9]+`.
///
/// node-zsync used an unanchored `zsync-tag-[0-9]+`; anchoring at the start avoids touching
/// snapshots that merely contain the pattern (e.g. `manual-zsync-hourly-1`).
pub fn tag_regex(prefix: &str, tag: Option<&str>) -> Regex {
    let mut stem = regex::escape(prefix);
    if let Some(t) = tag.filter(|t| !t.is_empty()) {
        stem.push('-');
        stem.push_str(&regex::escape(t));
    }
    Regex::new(&format!("^{stem}-[0-9]+")).expect("escaped pattern is valid")
}

/// Whether `snapshot` (short name) was created by zsync with `prefix` (any tag).
pub fn is_zsync_snapshot(prefix: &str, snapshot: &str) -> bool {
    snapshot.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('-'))
}

/// Split `pool/a@snap` (or `pool/a#bookmark`) into dataset and short name.
pub fn split_snapshot(full: &str) -> Option<(&str, &str)> {
    full.split_once('@').or_else(|| full.split_once('#'))
}

/// Name for a destination renamed out of the way by `--fallback-rename-destination`.
pub fn renamed_name(dest: &str, epoch_ms: u128) -> String {
    format!("{dest}-renamed-{epoch_ms}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_drop_matches_node() {
        // defaults: keep 1
        assert_eq!(name_drop("pool1/sub1/swap", None, None), "swap");
        // both explicit zero: full name
        assert_eq!(name_drop("pool1/sub1/swap", Some(0), Some(0)), "pool1/sub1/swap");
        // one explicit zero, other unset: keep 1
        assert_eq!(name_drop("pool1/sub1/swap", Some(0), None), "swap");
        assert_eq!(name_drop("pool1/sub1/swap", None, Some(0)), "swap");
        // drop from left
        assert_eq!(name_drop("pool1/sub1/swap", Some(1), None), "sub1/swap");
        assert_eq!(name_drop("pool1/sub1/swap", Some(5), None), "");
        // keep from right
        assert_eq!(name_drop("pool1/sub1/swap", None, Some(2)), "sub1/swap");
        assert_eq!(name_drop("pool1/sub1/swap", None, Some(9)), "pool1/sub1/swap");
        // drop then keep
        assert_eq!(name_drop("a/b/c/d", Some(1), Some(1)), "d");
        assert_eq!(name_drop("a/b/c/d", Some(1), Some(2)), "c/d");
        assert_eq!(name_drop("pool", None, None), "pool");
    }

    #[test]
    fn joins() {
        assert_eq!(dest_join("pool3", "sub1"), "pool3/sub1");
        assert_eq!(dest_join("pool3/", "/sub1"), "pool3/sub1");
        assert_eq!(dest_join("pool3", ""), "pool3");
        assert_eq!(destination_for("pool1/vm/disk0", "backup/vms", Some(1), None), "backup/vms/vm/disk0");
    }

    #[test]
    fn snapshot_names_and_tags() {
        assert_eq!(snapshot_name("zsync", Some("hourly"), "2024"), "zsync-hourly-2024");
        assert_eq!(snapshot_name("zsync", None, "2024"), "zsync-2024");
        assert_eq!(snapshot_name("zsync", Some(""), "2024"), "zsync-2024");

        let hourly = tag_regex("zsync", Some("hourly"));
        assert!(hourly.is_match("zsync-hourly-20240101"));
        assert!(!hourly.is_match("zsync-daily-20240101"));
        assert!(!hourly.is_match("manual-zsync-hourly-1"));
        assert!(!hourly.is_match("zsync-hourly-x"));

        let untagged = tag_regex("zsync", None);
        assert!(untagged.is_match("zsync-20240101"));
        assert!(!untagged.is_match("zsync-hourly-20240101"));

        let dotted = tag_regex("a.b", Some("x+y"));
        assert!(dotted.is_match("a.b-x+y-1"));
        assert!(!dotted.is_match("aXb-x+y-1"));

        assert!(is_zsync_snapshot("zsync", "zsync-hourly-1"));
        assert!(!is_zsync_snapshot("zsync", "zsyncx-1"));
        assert!(!is_zsync_snapshot("zsync", "manual"));
    }

    #[test]
    fn splitting() {
        assert_eq!(split_snapshot("p/a@s"), Some(("p/a", "s")));
        assert_eq!(split_snapshot("p/a#b"), Some(("p/a", "b")));
        assert_eq!(split_snapshot("p/a"), None);
        assert_eq!(renamed_name("p/a", 5), "p/a-renamed-5");
    }
}
