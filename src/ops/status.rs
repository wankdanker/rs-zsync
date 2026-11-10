//! `zsync status`: compare each selected dataset with its destination.

use super::{for_each, list_datasets, snapshots_of, Ctx, Destination, Outcome, Selection};
use crate::plan::{self, DestSide};
use anyhow::{anyhow, Result};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusRow {
    pub source: String,
    pub destination: String,
    pub destination_host: String,
    pub up_to_date: bool,
    /// Newest common snapshot (node-zsync's `fromSnap`), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// Planned action for push.
    pub action: plan::Action,
    pub resume_pending: bool,
    pub diff: plan::Diff,
}

pub fn diff_one(ctx: &Ctx, sel: &Selection, dest: &Destination, source: &str) -> Result<(plan::Diff, String, bool)> {
    let target = dest.for_source(source);
    crate::debug!("status(): requesting diff between {source} and {target} on {}", dest.host.label());
    let (src, marks) =
        snapshots_of(ctx, &sel.host, source, true)?.ok_or_else(|| anyhow!("dataset does not exist: {}:{source}", sel.host.label()))?;
    let side = match snapshots_of(ctx, &dest.host, &target, false)? {
        Some((snaps, _)) => DestSide { exists: true, snaps },
        None => DestSide { exists: false, snaps: vec![] },
    };
    let resume = side.exists && ctx.backend.get(&dest.host, &target, "receive_resume_token").ok().flatten().is_some();
    let d = plan::diff(source, &src, &marks, Some(&side)).map_err(|e| anyhow!(e))?;
    Ok((d, target, resume))
}

pub fn status(ctx: &Ctx, sel: &Selection, dest: &Destination, intermediary: bool, replication: bool) -> Result<Outcome<StatusRow>> {
    let datasets = list_datasets(ctx, sel)?;
    for_each(
        ctx,
        datasets,
        |d| d.name.clone(),
        |ds| {
            let (diff, target, resume_pending) = diff_one(ctx, sel, dest, &ds.name)?;
            Ok(StatusRow {
                source: ds.name.clone(),
                destination: target,
                destination_host: dest.host.label().to_string(),
                up_to_date: !diff.work,
                latest: diff.from_snap.clone(),
                action: plan::decide(&diff, intermediary, replication),
                resume_pending,
                diff,
            })
        },
    )
}
