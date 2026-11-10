//! The commands, as library functions. The CLI in `main.rs` only parses options into these
//! structs and prints the results.

pub mod admin;
pub mod push;
pub mod rotate;
pub mod snapshot;
pub mod status;
pub mod stream;

use crate::backend::Backend;
use crate::exec::is_not_found;
use crate::host::{Host, SshConfig};
use crate::plan::Snap;
use crate::select::Selector;
use crate::zfs::{Dataset, ListQuery};
use anyhow::{Context, Result};
use serde::Serialize;
use std::sync::Arc;

/// Settings every command shares.
#[derive(Clone)]
pub struct Ctx {
    pub ssh: Arc<SshConfig>,
    pub backend: Backend,
    /// `--continue`: keep going after a dataset fails.
    pub keep_going: bool,
    pub dry_run: bool,
}

impl Ctx {
    pub fn host(&self, name: Option<&str>) -> Host {
        Host::new(name, self.ssh.clone())
    }
}

/// Which datasets a command works on (node-zsync's `list`/`each` options).
#[derive(Clone, Debug)]
pub struct Selection {
    pub host: Host,
    /// `-s`: dataset to start from.
    pub source: Option<String>,
    /// `-t`: zfs types.
    pub types: Option<String>,
    /// `-R`: recurse from `source`.
    pub recursive: bool,
    pub glob: String,
    pub exclude: String,
}

/// Where datasets go.
#[derive(Clone, Debug)]
pub struct Destination {
    pub base: String,
    pub host: Host,
    pub drop: Option<usize>,
    pub keep: Option<usize>,
}

impl Destination {
    pub fn for_source(&self, source: &str) -> String {
        crate::naming::destination_for(source, &self.base, self.drop, self.keep)
    }
}

/// List datasets matching the selection, in `zfs list` order.
pub fn list_datasets(ctx: &Ctx, sel: &Selection) -> Result<Vec<Dataset>> {
    let query =
        ListQuery { root: sel.source.clone(), types: sel.types.clone(), recursive: sel.recursive, depth: None, sort_createtxg: false };
    crate::debug!(
        "list(): looking up datasets: type {:?}, source {}:{:?}, glob {:?}, exclude {:?}",
        sel.types,
        sel.host.label(),
        sel.source,
        sel.glob,
        sel.exclude
    );
    let rows = ctx.backend.list(&sel.host, &query).with_context(|| format!("cannot list datasets on {}", sel.host.label()))?;
    let selector = Selector::new(&sel.glob, &sel.exclude)?;
    Ok(selector.filter(rows, |d| d.name.as_str()))
}

/// Snapshots and bookmarks of exactly `dataset`, oldest first, or `None` if it does not exist.
pub fn snapshots_of(ctx: &Ctx, host: &Host, dataset: &str, bookmarks: bool) -> Result<Option<(Vec<Snap>, Vec<Snap>)>> {
    let rows = match ctx.backend.list(host, &ListQuery::snapshots_of(dataset, bookmarks)) {
        Ok(rows) => rows,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(e.context(format!("cannot list snapshots of {}:{dataset}", host.label()))),
    };
    let to_snap = |d: &Dataset| Snap { name: d.short().to_string(), guid: d.guid, createtxg: d.createtxg };
    let snaps = rows.iter().filter(|d| d.is_snapshot() && d.dataset() == dataset).map(to_snap).collect();
    let marks = rows.iter().filter(|d| d.is_bookmark() && d.dataset() == dataset).map(to_snap).collect();
    Ok(Some((snaps, marks)))
}

/// A per-dataset failure recorded under `--continue`.
#[derive(Clone, Debug, Serialize)]
pub struct Failure {
    pub dataset: String,
    pub error: String,
}

/// Results of a command over many datasets.
#[derive(Clone, Debug, Serialize)]
pub struct Outcome<T: Serialize> {
    pub results: Vec<T>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
}

/// Run `f` for each item. Without `--continue` the first error stops everything (as in
/// node-zsync); with it, errors are reported, recorded and the loop goes on.
pub fn for_each<I, T, F>(ctx: &Ctx, items: Vec<I>, name: impl Fn(&I) -> String, mut f: F) -> Result<Outcome<T>>
where
    T: Serialize,
    F: FnMut(&I) -> Result<T>,
{
    let mut out = Outcome { results: Vec::new(), failures: Vec::new() };
    for item in items {
        match f(&item) {
            Ok(r) => out.results.push(r),
            Err(e) if ctx.keep_going => {
                let msg = format!("{e:#}");
                eprintln!("zsync: {}: {msg}", name(&item));
                out.failures.push(Failure { dataset: name(&item), error: msg });
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}
