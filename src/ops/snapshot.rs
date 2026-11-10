//! `zsync snapshot`: snapshot every selected dataset.

use super::{for_each, list_datasets, Ctx, Outcome, Selection};
use crate::dateformat::format_now;
use crate::naming::snapshot_name;
use anyhow::Result;

#[derive(Clone, Debug)]
pub struct SnapOpts {
    pub prefix: String,
    pub tag: Option<String>,
    /// dateformat mask for the timestamp.
    pub date_format: String,
    /// Exact snapshot name instead of `prefix[-tag]-timestamp`.
    pub name: Option<String>,
    /// One `zfs snapshot` call for all datasets (atomic within a pool).
    pub atomic: bool,
}

fn full_name(dataset: &str, o: &SnapOpts, timestamp: &str) -> String {
    let short = o.name.clone().unwrap_or_else(|| snapshot_name(&o.prefix, o.tag.as_deref(), timestamp));
    format!("{dataset}@{short}")
}

pub fn snapshot(ctx: &Ctx, sel: &Selection, o: &SnapOpts) -> Result<Outcome<String>> {
    let datasets = list_datasets(ctx, sel)?;
    if o.atomic {
        let ts = format_now(&o.date_format);
        let names: Vec<String> = datasets.iter().map(|d| full_name(&d.name, o, &ts)).collect();
        for n in &names {
            crate::verbose!("snapshot {n} on {}", sel.host.label());
        }
        if !ctx.dry_run && !names.is_empty() {
            ctx.backend.snapshot(&sel.host, &names, false)?;
        }
        return Ok(super::Outcome { results: names, failures: vec![] });
    }
    for_each(
        ctx,
        datasets,
        |d| d.name.clone(),
        |d| {
            let name = full_name(&d.name, o, &format_now(&o.date_format));
            crate::verbose!("snapshot {name} on {}", sel.host.label());
            if !ctx.dry_run {
                ctx.backend.snapshot(&sel.host, std::slice::from_ref(&name), false)?;
            }
            Ok(name)
        },
    )
}
