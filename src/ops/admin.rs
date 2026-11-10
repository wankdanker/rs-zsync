//! `zsync destroy` and `zsync rename`.

use super::Ctx;
use crate::host::Host;
use crate::zfs::RenameOpts;
use anyhow::{bail, Result};

pub fn destroy(ctx: &Ctx, host: &Host, dataset: &str, recursive: bool) -> Result<()> {
    if dataset.is_empty() {
        bail!("a dataset to destroy is required");
    }
    crate::debug!("destroy(): destroying {dataset} on {}", host.label());
    if ctx.dry_run {
        crate::verbose!("would destroy {}:{dataset}", host.label());
        return Ok(());
    }
    ctx.backend.destroy(host, dataset, recursive)
}

pub fn rename(ctx: &Ctx, host: &Host, from: &str, to: &str, opts: &RenameOpts) -> Result<()> {
    if from.is_empty() || to.is_empty() {
        bail!("rename needs a source and a new name");
    }
    crate::debug!("rename(): renaming {from} on {} to {to}", host.label());
    if ctx.dry_run {
        crate::verbose!("would rename {}:{from} to {to}", host.label());
        return Ok(());
    }
    ctx.backend.rename(host, from, to, opts)
}
