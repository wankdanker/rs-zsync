//! `zsync push` / `zsync pull`: send each selected dataset's missing snapshots.

use super::{for_each, list_datasets, snapshots_of, Ctx, Destination, Outcome, Selection};
use crate::host::Host;
use crate::naming::{is_zsync_snapshot, renamed_name};
use crate::plan::{self, Action, DestSide, Snap};
use crate::transport::{self, DataPath, ProgressSink, Transfer, TransferError};
use crate::zfs::{RecvSpec, RenameOpts, SendSpec};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::time::Duration;

#[derive(Clone)]
pub struct PushOpts {
    pub dest: Destination,
    /// `-F`: let the receive roll back the destination.
    pub force: bool,
    /// `-r`: replication stream (`zfs send -R`).
    pub replication: bool,
    /// `-p`: include properties.
    pub properties: bool,
    /// Send intermediate snapshots (`-I`); `--latest` turns this off.
    pub intermediary: bool,
    pub compressed: bool,
    pub raw: bool,
    pub large_block: bool,
    /// `zfs receive -u`.
    pub unmounted: bool,
    pub fallback_destroy: bool,
    pub fallback_rename: bool,
    /// `zfs receive -s` and resume interrupted transfers.
    pub resumable: bool,
    /// Discard partial receive state instead of resuming.
    pub abort_resume: bool,
    pub retries: u32,
    /// Bookmark the sent snapshot on the source (keeps an incremental base after rotation).
    pub bookmark: bool,
    pub prefix: String,
    pub path: DataPath,
    pub progress: Option<ProgressSink>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushResult {
    pub source: String,
    pub destination: String,
    pub destination_host: String,
    /// `up-to-date`, `full`, `incremental`, or with `--dry-run` `would-...`.
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub replication: bool,
    pub resumed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
}

pub fn push(ctx: &Ctx, sel: &Selection, opts: &PushOpts) -> Result<Outcome<PushResult>> {
    if opts.dest.base.is_empty() {
        bail!("a destination is required (positional or -d/--destination)");
    }
    let datasets = list_datasets(ctx, sel)?;
    crate::debug!("push(): found {} source datasets to process", datasets.len());
    for_each(ctx, datasets, |d| d.name.clone(), |ds| sync_with_retries(ctx, &sel.host, &ds.name, opts))
}

fn sync_with_retries(ctx: &Ctx, src_host: &Host, source: &str, opts: &PushOpts) -> Result<PushResult> {
    let mut attempt = 0;
    loop {
        match sync_dataset(ctx, src_host, source, opts) {
            Err(e) if attempt < opts.retries && e.downcast_ref::<TransferError>().is_some() => {
                attempt += 1;
                let wait = Duration::from_secs((1u64 << attempt.min(6)).min(60));
                eprintln!("zsync: {source}: {e:#}; retry {attempt}/{} in {}s", opts.retries, wait.as_secs());
                std::thread::sleep(wait);
            }
            other => return other,
        }
    }
}

fn dest_side(ctx: &Ctx, host: &Host, dataset: &str) -> Result<DestSide> {
    Ok(match snapshots_of(ctx, host, dataset, false)? {
        Some((snaps, _)) => DestSide { exists: true, snaps },
        None => DestSide { exists: false, snaps: vec![] },
    })
}

fn sync_dataset(ctx: &Ctx, src_host: &Host, source: &str, opts: &PushOpts) -> Result<PushResult> {
    let dst_host = &opts.dest.host;
    let target = opts.dest.for_source(source);
    crate::debug!("push(): requesting diff between {}:{source} and {}:{target}", src_host.label(), dst_host.label());

    let (src_snaps, src_marks) =
        snapshots_of(ctx, src_host, source, true)?.ok_or_else(|| anyhow!("dataset does not exist: {}:{source}", src_host.label()))?;
    let mut side = dest_side(ctx, dst_host, &target)?;
    let mut result = PushResult {
        source: source.into(),
        destination: target.clone(),
        destination_host: dst_host.label().into(),
        action: String::new(),
        from: None,
        to: None,
        replication: false,
        resumed: false,
        fallback: None,
        bytes: None,
        seconds: None,
    };

    // Finish (or discard) an interrupted receive first.
    if side.exists && (opts.resumable || opts.abort_resume) {
        if let Some(token) = ctx.backend.get(dst_host, &target, "receive_resume_token")? {
            if opts.abort_resume {
                crate::verbose!("{target}: discarding partially received state");
                if !ctx.dry_run {
                    ctx.backend.recv_abort(dst_host, &target)?;
                }
            } else if ctx.dry_run {
                crate::verbose!("{target}: would resume an interrupted receive");
                result.resumed = true;
            } else {
                crate::verbose!("{source} -> {}:{target}: resuming interrupted receive", dst_host.label());
                let spec = SendSpec { resume_token: Some(token), ..Default::default() };
                let recv = RecvSpec { dataset: target.clone(), resumable: true, unmounted: opts.unmounted, ..Default::default() };
                let stats = move_stream(ctx, src_host, opts, &target, spec, recv)?;
                result.resumed = true;
                result.bytes = Some(stats.bytes);
            }
            side = dest_side(ctx, dst_host, &target)?;
        }
    }

    let diff = plan::diff(source, &src_snaps, &src_marks, Some(&side)).map_err(|e| anyhow!(e))?;
    result.to = Some(diff.to_snap.clone());
    let mut action = plan::decide(&diff, opts.intermediary, opts.replication);

    if action == Action::Diverged {
        let renamed = renamed_name(&target, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0));
        if opts.fallback_destroy {
            crate::verbose!("{target}: no common snapshot with {source}; destroying destination (--fallback-destroy-destination)");
            if !ctx.dry_run {
                ctx.backend.destroy(dst_host, &target, true)?;
            }
            result.fallback = Some("destroyed".into());
        } else if opts.fallback_rename {
            crate::verbose!("{target}: no common snapshot with {source}; renaming destination to {renamed}");
            if !ctx.dry_run {
                ctx.backend.rename(dst_host, &target, &renamed, &RenameOpts::default())?;
            }
            result.fallback = Some(format!("renamed to {renamed}"));
        } else {
            bail!(
                "Destination {}:{target} has snapshots which are not in common with source {source}. Receive will fail. \
                 Use --fallback-rename-destination or --fallback-destroy-destination.",
                dst_host.label()
            );
        }
        action = Action::Full { replication: true };
    }

    let (from, intermediary, replication) = match &action {
        Action::UpToDate => {
            crate::verbose!("{source} -> {}:{target}: up to date", dst_host.label());
            result.action = "up-to-date".into();
            result.from = diff.from_snap.clone();
            return Ok(result);
        }
        Action::Full { replication } => (None, false, *replication),
        Action::Incremental { from, intermediary } => (Some(from.clone()), *intermediary, opts.replication),
        Action::Diverged => unreachable!("handled above"),
    };
    result.action = if from.is_some() { "incremental" } else { "full" }.into();
    result.from = from.clone();
    result.replication = replication;

    let spec = SendSpec {
        snapshot: diff.to_snap.clone(),
        from: from.clone(),
        intermediary,
        replication,
        properties: opts.properties,
        compressed: opts.compressed,
        raw: opts.raw,
        large_block: opts.large_block,
        ..Default::default()
    };
    // Resumable receives don't combine with replication streams.
    let recv = RecvSpec {
        dataset: target.clone(),
        force: opts.force,
        resumable: opts.resumable && !replication,
        unmounted: opts.unmounted,
        verbose: false,
    };
    crate::verbose!(
        "{source} -> {}:{target}: {} send {}{} ({} snapshot(s))",
        dst_host.label(),
        result.action,
        from.as_deref().map(|f| format!("{f} .. ")).unwrap_or_default(),
        diff.to_snap,
        if from.is_some() && !intermediary { 1 } else { diff.missing.len() }
    );
    if ctx.dry_run {
        result.action = format!("would-{}", result.action);
        return Ok(result);
    }

    let stats = move_stream(ctx, src_host, opts, &target, spec, recv)?;
    result.bytes = Some(stats.bytes + result.bytes.unwrap_or(0));
    result.seconds = Some(stats.seconds);

    if opts.bookmark {
        bookmark_sent(ctx, src_host, source, &diff.to_snap, &src_marks, &opts.prefix)?;
    }
    Ok(result)
}

fn move_stream(ctx: &Ctx, src_host: &Host, opts: &PushOpts, target: &str, mut spec: SendSpec, recv: RecvSpec) -> Result<transport::Stats> {
    let estimate = match &opts.progress {
        Some(_) => ctx.backend.estimate(src_host, &spec).unwrap_or_else(|e| {
            crate::debug!("estimate failed: {e:#}");
            None
        }),
        None => None,
    };
    spec.progress = opts.progress.is_some() && opts.path.needs_send_progress();
    let t = Transfer {
        source: src_host,
        destination: &opts.dest.host,
        send: ctx.backend.send_stage(src_host, &spec),
        recv: ctx.backend.recv_stage(&opts.dest.host, &recv),
        path: &opts.path,
        backend: &ctx.backend,
        label: target.to_string(),
        estimate,
        progress: opts.progress.clone(),
    };
    transport::transfer(&t)
}

/// Create `source#<snapshot>` and remove older zsync bookmarks of the dataset.
fn bookmark_sent(ctx: &Ctx, host: &Host, source: &str, to_snap: &str, existing: &[Snap], prefix: &str) -> Result<()> {
    let short = to_snap.rsplit_once('@').map(|(_, s)| s).unwrap_or(to_snap);
    let name = format!("{source}#{short}");
    if !existing.iter().any(|b| b.name == short) {
        crate::verbose!("bookmarking {to_snap} as {name}");
        ctx.backend.bookmark(host, to_snap, &name)?;
    }
    for old in existing.iter().filter(|b| b.name != short && is_zsync_snapshot(prefix, &b.name)) {
        crate::verbose!("destroying old bookmark {source}#{}", old.name);
        ctx.backend.destroy(host, &format!("{source}#{}", old.name), false)?;
    }
    Ok(())
}
