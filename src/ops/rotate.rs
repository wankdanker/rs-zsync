//! `zsync rotate` (keep the newest N tagged snapshots) and `zsync prune` (remove destination
//! snapshots that are gone from the source).

use super::{for_each, list_datasets, snapshots_of, Ctx, Destination, Outcome, Selection};
use crate::naming::{is_zsync_snapshot, tag_regex};
use crate::plan::{self, same, Snap};
use anyhow::{bail, Result};
use serde::Serialize;

#[derive(Clone, Debug)]
pub struct RotateOpts {
    pub prefix: String,
    pub tag: Option<String>,
    /// Number of snapshots to keep (required unless `snapshot` is given).
    pub keep: usize,
    /// Destroy exactly this snapshot (short name) instead of rotating.
    pub snapshot: Option<String>,
    /// Never destroy the newest snapshot the destination also has.
    pub preserve_incremental: bool,
    /// Rotate the destination's snapshots instead of the source's.
    pub destination_side: bool,
    pub dest: Option<Destination>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Destroyed {
    pub dataset: String,
    pub host: String,
    pub snapshots: Vec<String>,
}

pub fn rotate(ctx: &Ctx, sel: &Selection, o: &RotateOpts) -> Result<Outcome<Destroyed>> {
    if o.keep == 0 && o.snapshot.is_none() {
        bail!("You must specify a value for keep.");
    }
    if (o.preserve_incremental || o.destination_side) && o.dest.as_ref().map_or(true, |d| d.base.is_empty()) {
        bail!("--preserve-incremental and --destination-side need a destination");
    }
    let re = tag_regex(&o.prefix, o.tag.as_deref());
    let datasets = list_datasets(ctx, sel)?;
    for_each(
        ctx,
        datasets,
        |d| d.name.clone(),
        |ds| {
            let source = ds.name.as_str();
            let src = snapshots_of(ctx, &sel.host, source, false)?.map(|s| s.0).unwrap_or_default();
            let dest = o.dest.as_ref().filter(|d| !d.base.is_empty());
            let target = dest.map(|d| d.for_source(source));
            let dst: Option<Vec<Snap>> = match (dest, &target) {
                (Some(d), Some(t)) if o.preserve_incremental || o.destination_side => snapshots_of(ctx, &d.host, t, false)?.map(|s| s.0),
                _ => None,
            };

            // Operate on the source, or on the destination with --destination-side.
            let (host, dataset, own, other) = if o.destination_side {
                let d = dest.expect("checked above");
                (d.host.clone(), target.clone().unwrap(), dst.clone().unwrap_or_default(), Some(src.clone()))
            } else {
                (sel.host.clone(), source.to_string(), src.clone(), if o.preserve_incremental { dst.clone() } else { None })
            };
            let matching: Vec<Snap> = own.iter().filter(|s| re.is_match(&s.name)).cloned().collect();
            let other_matching: Option<Vec<Snap>> = other.as_ref().map(|v| v.iter().filter(|s| re.is_match(&s.name)).cloned().collect());
            crate::debug!("rotate(): {dataset}: {} snapshots, {} matching {}", own.len(), matching.len(), re.as_str());

            let victims: Vec<String> = match &o.snapshot {
                Some(name) => {
                    let Some(snap) = own.iter().find(|s| &s.name == name) else {
                        return Ok(Destroyed { dataset, host: host.label().into(), snapshots: vec![] });
                    };
                    // Refuse to remove the incremental base (whatever its tag).
                    let base = other.as_ref().and_then(|o| own.iter().rev().find(|s| o.iter().any(|x| same(s, x))));
                    if base.is_some_and(|b| same(b, snap)) {
                        eprintln!("zsync: {dataset}@{name} is the newest snapshot in common with the destination; not destroying it");
                        vec![]
                    } else {
                        vec![name.clone()]
                    }
                }
                None => plan::rotation(&matching, o.keep, other_matching.as_deref()),
            };
            destroy_all(ctx, &host, &dataset, &victims)?;
            Ok(Destroyed { dataset, host: host.label().into(), snapshots: victims })
        },
    )
}

fn destroy_all(ctx: &Ctx, host: &crate::host::Host, dataset: &str, names: &[String]) -> Result<()> {
    for name in names {
        crate::verbose!("destroy {}:{dataset}@{name}", host.label());
        if !ctx.dry_run {
            ctx.backend.destroy(host, &format!("{dataset}@{name}"), false)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct PruneOpts {
    pub prefix: String,
    pub tag: Option<String>,
    /// Consider every snapshot, not only zsync-named ones.
    pub all: bool,
    pub dest: Destination,
}

pub fn prune(ctx: &Ctx, sel: &Selection, o: &PruneOpts) -> Result<Outcome<Destroyed>> {
    if o.dest.base.is_empty() {
        bail!("a destination is required (positional or -d/--destination)");
    }
    let tag = o.tag.as_deref().filter(|t| !t.is_empty()).map(|t| tag_regex(&o.prefix, Some(t)));
    let datasets = list_datasets(ctx, sel)?;
    for_each(
        ctx,
        datasets,
        |d| d.name.clone(),
        |ds| {
            let target = o.dest.for_source(&ds.name);
            let host = o.dest.host.label().to_string();
            let empty = || Destroyed { dataset: target.clone(), host: host.clone(), snapshots: vec![] };
            let Some((src, _)) = snapshots_of(ctx, &sel.host, &ds.name, false)? else { return Ok(empty()) };
            let Some((dst, _)) = snapshots_of(ctx, &o.dest.host, &target, false)? else { return Ok(empty()) };
            if !dst.iter().any(|d| src.iter().any(|s| same(s, d))) {
                if !dst.is_empty() {
                    eprintln!("zsync: {host}:{target} has no snapshot in common with {}; not pruning it", ds.name);
                }
                return Ok(empty());
            }
            let victims =
                plan::prune(&src, &dst, |n| (o.all || is_zsync_snapshot(&o.prefix, n)) && tag.as_ref().map_or(true, |re| re.is_match(n)));
            destroy_all(ctx, &o.dest.host, &target, &victims)?;
            Ok(Destroyed { dataset: target.clone(), host: host.clone(), snapshots: victims })
        },
    )
}
