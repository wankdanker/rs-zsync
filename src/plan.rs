//! The core of zsync, free of I/O: compare a source's snapshots with a destination's, find the
//! newest snapshot they share, and decide what to send.
//!
//! Snapshots are matched by GUID when both sides report one (so a renamed snapshot still counts
//! as common and a same-named but different snapshot does not), falling back to the name.

use serde::{Deserialize, Serialize};

/// A snapshot or bookmark, by short name (the part after `@`/`#`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snap {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub createtxg: Option<u64>,
}

impl Snap {
    pub fn named(name: &str) -> Snap {
        Snap { name: name.into(), guid: None, createtxg: None }
    }
    pub fn with_guid(name: &str, guid: u64) -> Snap {
        Snap { name: name.into(), guid: Some(guid), createtxg: None }
    }
}

/// Whether two snapshots are the same: equal GUIDs when both are known, else equal names.
pub fn same(a: &Snap, b: &Snap) -> bool {
    match (a.guid, b.guid) {
        (Some(x), Some(y)) => x == y,
        _ => a.name == b.name,
    }
}

/// The destination side as seen before a sync.
#[derive(Clone, Debug, Default)]
pub struct DestSide {
    pub exists: bool,
    /// Oldest first.
    pub snaps: Vec<Snap>,
}

/// Result of comparing source and destination (node-zsync's `diff`).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Diff {
    pub source: String,
    pub source_snapshots: Vec<String>,
    pub destination_snapshots: Vec<String>,
    pub destination_exists: bool,
    /// `source@newest`.
    pub to_snap: String,
    /// `source@common` (or `source#bookmark`) when there is a common base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_snap: Option<String>,
    pub from_bookmark: bool,
    /// Whether anything needs sending.
    pub work: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Source snapshots newer than the common base (what an `-I` send would carry).
    pub missing: Vec<String>,
    /// Destination snapshots newer than the common base; receiving needs `--force` (rollback).
    pub destination_extra: Vec<String>,
}

/// Compare. `source_snaps` and `source_bookmarks` are the source dataset's own, oldest first.
/// `dest` is `None` when there is no destination to compare against.
pub fn diff(source: &str, source_snaps: &[Snap], source_bookmarks: &[Snap], dest: Option<&DestSide>) -> Result<Diff, String> {
    let Some(newest) = source_snaps.last() else {
        return Err(format!("No local snapshots exist for {source}"));
    };
    let mut d = Diff {
        source: source.into(),
        source_snapshots: source_snaps.iter().map(|s| s.name.clone()).collect(),
        destination_snapshots: dest.map(|d| d.snaps.iter().map(|s| s.name.clone()).collect()).unwrap_or_default(),
        destination_exists: dest.map(|d| d.exists).unwrap_or(false),
        to_snap: format!("{source}@{}", newest.name),
        from_snap: None,
        from_bookmark: false,
        work: true,
        message: None,
        missing: Vec::new(),
        destination_extra: Vec::new(),
    };

    let Some(dest) = dest.filter(|d| d.exists) else {
        d.missing = d.source_snapshots.clone();
        return Ok(d);
    };

    // Newest destination snapshot that also exists on the source.
    let common = dest.snaps.iter().enumerate().rev().find_map(|(di, ds)| {
        source_snaps.iter().position(|s| same(s, ds)).map(|si| (di, si))
    });

    match common {
        Some((di, si)) => {
            d.from_snap = Some(format!("{source}@{}", source_snaps[si].name));
            d.missing = source_snaps[si + 1..].iter().map(|s| s.name.clone()).collect();
            d.destination_extra = dest.snaps[di + 1..].iter().map(|s| s.name.clone()).collect();
            if si == source_snaps.len() - 1 {
                d.work = false;
                d.message = Some("Destination has most recent snapshot".into());
            }
        }
        None => {
            // A bookmark can stand in for a destroyed source snapshot (only GUIDs can tell).
            let via_bookmark = dest.snaps.iter().enumerate().rev().find_map(|(di, ds)| {
                ds.guid?;
                source_bookmarks.iter().find(|b| b.guid == ds.guid).map(|b| (di, b, ds))
            });
            if let Some((di, b, ds)) = via_bookmark {
                d.from_snap = Some(format!("{source}#{}", b.name));
                d.from_bookmark = true;
                d.destination_extra = dest.snaps[di + 1..].iter().map(|s| s.name.clone()).collect();
                // Source snapshots created after the bookmarked one (by txg when known).
                d.missing = match ds.createtxg.and(b.createtxg) {
                    Some(txg) => source_snaps.iter().filter(|s| s.createtxg.is_some_and(|t| t > txg)).map(|s| s.name.clone()).collect(),
                    None => vec![newest.name.clone()],
                };
            } else {
                d.missing = d.source_snapshots.clone();
            }
        }
    }
    Ok(d)
}

/// What push should do with a dataset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Action {
    /// Destination already has the newest snapshot.
    UpToDate,
    /// No common snapshot and nothing on the destination: send everything.
    Full { replication: bool },
    /// Send from the common base.
    Incremental { from: String, intermediary: bool },
    /// Destination has snapshots but none in common with the source.
    Diverged,
}

/// node-zsync's push rules:
/// * no work -> up to date;
/// * a common base -> incremental (`-I` unless `--latest`; a bookmark base can only do `-i`);
/// * destination has snapshots but none in common -> diverged (needs a fallback);
/// * otherwise full; a replication stream is forced unless `--latest` (so intermediate
///   snapshots and properties come along), or when explicitly requested.
pub fn decide(d: &Diff, intermediary: bool, replication: bool) -> Action {
    if !d.work {
        return Action::UpToDate;
    }
    if let Some(from) = &d.from_snap {
        return Action::Incremental { from: from.clone(), intermediary: intermediary && !d.from_bookmark };
    }
    if !d.destination_snapshots.is_empty() {
        return Action::Diverged;
    }
    Action::Full { replication: replication || intermediary }
}

/// Snapshots rotate should destroy.
///
/// `candidates` are the source snapshots matching the tag pattern, oldest first. With
/// `preserve`, the newest of them that is common with `dest_matching` is kept and counts toward
/// `keep` (node-zsync's `--preserve-incremental`).
pub fn rotation(candidates: &[Snap], keep: usize, preserve: Option<&[Snap]>) -> Vec<String> {
    let mut list: Vec<&Snap> = candidates.iter().collect();
    let mut keep = keep;
    if let Some(dest) = preserve {
        if let Some(pos) = list.iter().rposition(|s| dest.iter().any(|d| same(s, d))) {
            list.remove(pos);
            keep = keep.saturating_sub(1);
        }
    }
    let n = list.len().saturating_sub(keep);
    list[..n].iter().map(|s| s.name.clone()).collect()
}

/// Destination snapshots that no longer exist on the source, never including the newest common
/// snapshot (the incremental base). `eligible` filters by name (prefix/tag).
pub fn prune<F: Fn(&str) -> bool>(source: &[Snap], dest: &[Snap], eligible: F) -> Vec<String> {
    dest.iter().filter(|d| !source.iter().any(|s| same(s, d)) && eligible(&d.name)).map(|d| d.name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snaps(names: &[&str]) -> Vec<Snap> {
        names.iter().map(|n| Snap::named(n)).collect()
    }

    fn dest(names: &[&str]) -> DestSide {
        DestSide { exists: true, snaps: snaps(names) }
    }

    #[test]
    fn no_source_snapshots_is_an_error() {
        let e = diff("p/a", &[], &[], None).unwrap_err();
        assert_eq!(e, "No local snapshots exist for p/a");
    }

    #[test]
    fn no_destination_compare() {
        let d = diff("p/a", &snaps(&["1", "2"]), &[], None).unwrap();
        assert_eq!(d.to_snap, "p/a@2");
        assert_eq!(d.from_snap, None);
        assert!(d.work);
        assert!(!d.destination_exists);
        assert_eq!(decide(&d, true, false), Action::Full { replication: true });
        assert_eq!(decide(&d, false, false), Action::Full { replication: false });
        assert_eq!(decide(&d, false, true), Action::Full { replication: true });
    }

    #[test]
    fn missing_destination_dataset() {
        let side = DestSide { exists: false, snaps: vec![] };
        let d = diff("p/a", &snaps(&["1"]), &[], Some(&side)).unwrap();
        assert_eq!(d.missing, vec!["1"]);
        assert_eq!(decide(&d, true, false), Action::Full { replication: true });
    }

    #[test]
    fn up_to_date() {
        let d = diff("p/a", &snaps(&["1", "2"]), &[], Some(&dest(&["1", "2"]))).unwrap();
        assert!(!d.work);
        assert_eq!(d.message.as_deref(), Some("Destination has most recent snapshot"));
        assert_eq!(d.from_snap.as_deref(), Some("p/a@2"));
        assert_eq!(decide(&d, true, false), Action::UpToDate);
    }

    #[test]
    fn incremental_from_newest_common() {
        let d = diff("p/a", &snaps(&["1", "2", "3", "4"]), &[], Some(&dest(&["1", "2"]))).unwrap();
        assert_eq!(d.from_snap.as_deref(), Some("p/a@2"));
        assert_eq!(d.to_snap, "p/a@4");
        assert_eq!(d.missing, vec!["3", "4"]);
        assert_eq!(decide(&d, true, false), Action::Incremental { from: "p/a@2".into(), intermediary: true });
        assert_eq!(decide(&d, false, false), Action::Incremental { from: "p/a@2".into(), intermediary: false });
    }

    #[test]
    fn non_sequential_snapshot_sets() {
        // Source rotated away "1"; destination kept older ones and missed "3".
        let d = diff("p/a", &snaps(&["2", "4", "5"]), &[], Some(&dest(&["0", "1", "2", "3"]))).unwrap();
        assert_eq!(d.from_snap.as_deref(), Some("p/a@2"));
        assert_eq!(d.destination_extra, vec!["3"]);
        assert_eq!(d.missing, vec!["4", "5"]);
    }

    #[test]
    fn newest_common_scans_destination_from_newest() {
        let d = diff("p/a", &snaps(&["1", "2", "3"]), &[], Some(&dest(&["1", "x", "2", "y"]))).unwrap();
        assert_eq!(d.from_snap.as_deref(), Some("p/a@2"));
        assert_eq!(d.destination_extra, vec!["y"]);
    }

    #[test]
    fn diverged() {
        let d = diff("p/a", &snaps(&["1", "2"]), &[], Some(&dest(&["x", "y"]))).unwrap();
        assert_eq!(d.from_snap, None);
        assert_eq!(decide(&d, true, false), Action::Diverged);
    }

    #[test]
    fn existing_destination_without_snapshots_gets_full_send() {
        let d = diff("p/a", &snaps(&["1"]), &[], Some(&dest(&[]))).unwrap();
        assert_eq!(decide(&d, true, false), Action::Full { replication: true });
    }

    #[test]
    fn guid_beats_name() {
        // Same name, different snapshot: not common.
        let src = vec![Snap::with_guid("a", 1), Snap::with_guid("b", 2)];
        let dst = DestSide { exists: true, snaps: vec![Snap::with_guid("b", 99)] };
        let d = diff("p/x", &src, &[], Some(&dst)).unwrap();
        assert_eq!(decide(&d, true, false), Action::Diverged);
        // Renamed on the destination, same guid: common.
        let dst = DestSide { exists: true, snaps: vec![Snap::with_guid("renamed", 1)] };
        let d = diff("p/x", &src, &[], Some(&dst)).unwrap();
        assert_eq!(d.from_snap.as_deref(), Some("p/x@a"));
        // GUID only known on one side: fall back to names.
        let dst = DestSide { exists: true, snaps: vec![Snap::named("a")] };
        let d = diff("p/x", &src, &[], Some(&dst)).unwrap();
        assert_eq!(d.from_snap.as_deref(), Some("p/x@a"));
    }

    #[test]
    fn bookmark_base() {
        let src = vec![Snap { name: "c".into(), guid: Some(3), createtxg: Some(30) }];
        let bms = vec![Snap { name: "b".into(), guid: Some(2), createtxg: Some(20) }];
        let dst = DestSide {
            exists: true,
            snaps: vec![Snap { name: "a".into(), guid: Some(1), createtxg: Some(5) }, Snap { name: "b".into(), guid: Some(2), createtxg: Some(6) }],
        };
        let d = diff("p/x", &src, &bms, Some(&dst)).unwrap();
        assert_eq!(d.from_snap.as_deref(), Some("p/x#b"));
        assert!(d.from_bookmark);
        assert_eq!(d.missing, vec!["c"]);
        assert_eq!(decide(&d, true, false), Action::Incremental { from: "p/x#b".into(), intermediary: false });
        // a real common snapshot is preferred over a bookmark
        let src2 = vec![Snap { name: "b".into(), guid: Some(2), createtxg: Some(20) }, src[0].clone()];
        let d = diff("p/x", &src2, &bms, Some(&dst)).unwrap();
        assert_eq!(d.from_snap.as_deref(), Some("p/x@b"));
        assert!(!d.from_bookmark);
    }

    #[test]
    fn rotation_keeps_newest() {
        let c = snaps(&["1", "2", "3", "4", "5"]);
        assert_eq!(rotation(&c, 2, None), vec!["1", "2", "3"]);
        assert_eq!(rotation(&c, 5, None), Vec::<String>::new());
        assert_eq!(rotation(&c, 9, None), Vec::<String>::new());
        assert_eq!(rotation(&c, 0, None), vec!["1", "2", "3", "4", "5"]);
    }

    #[test]
    fn rotation_preserves_incremental_base() {
        let c = snaps(&["1", "2", "3", "4", "5"]);
        // common base "2" survives and counts toward keep
        let d = snaps(&["1", "2"]);
        assert_eq!(rotation(&c, 2, Some(&d)), vec!["1", "3", "4"]);
        // base already among the newest: same as plain rotation
        let d = snaps(&["5"]);
        assert_eq!(rotation(&c, 2, Some(&d)), vec!["1", "2", "3"]);
        // nothing in common: plain rotation
        let d = snaps(&["x"]);
        assert_eq!(rotation(&c, 2, Some(&d)), vec!["1", "2", "3"]);
        // keep 1 with a common base keeps only the base (node-zsync semantics)
        let d = snaps(&["3"]);
        assert_eq!(rotation(&c, 1, Some(&d)), vec!["1", "2", "4", "5"]);
    }

    #[test]
    fn pruning() {
        let s = snaps(&["zsync-3", "zsync-4"]);
        let d = snaps(&["zsync-1", "manual", "zsync-3", "zsync-4", "zsync-9"]);
        let out = prune(&s, &d, |n| n.starts_with("zsync-"));
        assert_eq!(out, vec!["zsync-1", "zsync-9"]);
        let out = prune(&s, &d, |_| true);
        assert_eq!(out, vec!["zsync-1", "manual", "zsync-9"]);
    }

    /// Exhaustive check over small universes: the chosen base is always common, always the newest
    /// common one, and `missing` is exactly what follows it on the source.
    #[test]
    fn exhaustive_small_universes() {
        let universe = ["a", "b", "c", "d"];
        let subsets: Vec<Vec<&str>> = (0..16u32)
            .map(|m| universe.iter().enumerate().filter(|(i, _)| m & (1 << i) != 0).map(|(_, s)| *s).collect())
            .collect();
        for src in subsets.iter().filter(|s| !s.is_empty()) {
            for dst in &subsets {
                let d = diff("p", &snaps(src), &[], Some(&dest(dst))).unwrap();
                let common: Vec<&str> = dst.iter().copied().filter(|x| src.contains(x)).collect();
                match common.last() {
                    Some(c) => {
                        assert_eq!(d.from_snap, Some(format!("p@{c}")));
                        let pos = src.iter().position(|s| s == c).unwrap();
                        assert_eq!(d.missing, src[pos + 1..].iter().map(|s| s.to_string()).collect::<Vec<_>>());
                        assert_eq!(d.work, pos + 1 != src.len());
                    }
                    None => {
                        assert_eq!(d.from_snap, None);
                        let a = decide(&d, true, false);
                        if dst.is_empty() {
                            assert_eq!(a, Action::Full { replication: true });
                        } else {
                            assert_eq!(a, Action::Diverged);
                        }
                    }
                }
            }
        }
    }
}
