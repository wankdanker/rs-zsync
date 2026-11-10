//! How zsync talks to ZFS on a host.
//!
//! * **Agentless** (default): run `zfs ...` directly (locally, or through ssh) and parse its
//!   output here. Only the machine running zsync needs zsync installed.
//! * **Remote zsync** (`--remote-zsync`): like node-zsync, run `zsync rpc <json>` on remote hosts
//!   and read a JSON envelope back; data streams go through `zsync send` / `zsync receive` there.
//!   Local operations are always agentless.

use crate::exec::{capture, run, run_script, CommandError};
use crate::host::{Host, Script};
use crate::zfs::{self, Dataset, ListQuery, RecvSpec, RenameOpts, SendSpec};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A metadata request, as sent to `zsync rpc`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    List { query: ListQuery },
    Snapshot { names: Vec<String>, recursive: bool },
    Destroy { name: String, recursive: bool },
    Rename { from: String, to: String, opts: RenameOpts },
    Bookmark { snapshot: String, bookmark: String },
    Get { dataset: String, property: String },
    RecvAbort { dataset: String },
    Estimate { spec: SendSpec },
    Version,
}

/// The JSON envelope every `--format json` command and `zsync rpc` prints.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub message: String,
}

impl Envelope {
    pub fn ok(result: Value) -> Envelope {
        Envelope { version: 1, ok: true, result, error: None }
    }
    pub fn err(message: String) -> Envelope {
        Envelope { version: 1, ok: false, result: Value::Null, error: Some(ErrorBody { message }) }
    }
}

#[derive(Clone, Debug)]
pub enum Mode {
    Agentless,
    RemoteZsync { zsync: String },
}

#[derive(Clone, Debug)]
pub struct Backend {
    /// `zfs` command words, e.g. `["zfs"]` or `["sudo", "-n", "zfs"]`.
    pub zfs: Vec<String>,
    pub mode: Mode,
}

impl Default for Backend {
    fn default() -> Self {
        Backend { zfs: vec!["zfs".into()], mode: Mode::Agentless }
    }
}

impl Backend {
    fn rpc_target<'a>(&'a self, host: &Host) -> Option<&'a str> {
        match (&self.mode, host.is_local()) {
            (Mode::RemoteZsync { zsync }, false) => Some(zsync.as_str()),
            _ => None,
        }
    }

    /// The zsync command words for `host` (remote path, or this executable locally).
    pub fn zsync_words(&self, host: &Host) -> Vec<String> {
        match (&self.mode, host.is_local()) {
            (Mode::RemoteZsync { zsync }, false) => crate::util::split_words(zsync),
            (_, true) => vec![std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "zsync".into())],
            _ => vec!["zsync".into()],
        }
    }

    fn rpc(&self, host: &Host, zsync: &str, req: &Request) -> Result<Value> {
        let mut argv = crate::util::split_words(zsync);
        argv.extend(["rpc".into(), "--zfs-path".into(), self.zfs.join(" "), serde_json::to_string(req)?]);
        // The envelope carries the error message even when the remote exits non-zero.
        let (code, out, stderr) = capture(host, &Script::Argv(argv.clone()))?;
        let env: Envelope = match serde_json::from_str(out.trim()) {
            Ok(env) => env,
            Err(_) if code != Some(0) => {
                return Err(CommandError { host: host.label().into(), command: crate::util::shell_join(&argv), code, stderr }.into())
            }
            Err(e) => return Err(anyhow!("invalid rpc response from {}: {e}: {}", host.label(), out.trim())),
        };
        if !env.ok {
            bail!("{}: {}", host.label(), env.error.map(|e| e.message).unwrap_or_else(|| "remote error".into()));
        }
        Ok(env.result)
    }

    /// Execute a request locally (what `zsync rpc` does on the remote side), or on `host`.
    pub fn execute(&self, host: &Host, req: &Request) -> Result<Value> {
        if let Some(zsync) = self.rpc_target(host) {
            return self.rpc(host, zsync, req);
        }
        let z = &self.zfs;
        Ok(match req {
            Request::List { query } => {
                let out = run(host, &zfs::list_argv(z, query))?;
                serde_json::to_value(zfs::parse_list(&out)?)?
            }
            Request::Snapshot { names, recursive } => {
                if names.is_empty() {
                    return Ok(Value::Null);
                }
                run(host, &zfs::snapshot_argv(z, names, *recursive))?;
                Value::Null
            }
            Request::Destroy { name, recursive } => {
                run(host, &zfs::destroy_argv(z, name, *recursive))?;
                Value::Null
            }
            Request::Rename { from, to, opts } => {
                run(host, &zfs::rename_argv(z, from, to, opts))?;
                Value::Null
            }
            Request::Bookmark { snapshot, bookmark } => {
                run(host, &zfs::bookmark_argv(z, snapshot, bookmark))?;
                Value::Null
            }
            Request::Get { dataset, property } => {
                let out = run(host, &zfs::get_argv(z, property, dataset))?;
                serde_json::to_value(zfs::parse_get(&out))?
            }
            Request::RecvAbort { dataset } => {
                run(host, &zfs::recv_abort_argv(z, dataset))?;
                Value::Null
            }
            Request::Estimate { spec } => {
                let mut spec = spec.clone();
                spec.dry_run = true;
                // Capture stdout and stderr together: versions differ in where -nvP output goes.
                let script = Script::Shell(format!("{} 2>&1", crate::util::shell_join(&zfs::send_argv(z, &spec))));
                let out = run_script(host, &script, None)?;
                serde_json::to_value(zfs::parse_estimate(&out))?
            }
            Request::Version => Value::String(env!("CARGO_PKG_VERSION").into()),
        })
    }

    pub fn list(&self, host: &Host, query: &ListQuery) -> Result<Vec<Dataset>> {
        let v = self.execute(host, &Request::List { query: query.clone() })?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn snapshot(&self, host: &Host, names: &[String], recursive: bool) -> Result<()> {
        self.execute(host, &Request::Snapshot { names: names.to_vec(), recursive }).map(|_| ())
    }

    pub fn destroy(&self, host: &Host, name: &str, recursive: bool) -> Result<()> {
        self.execute(host, &Request::Destroy { name: name.into(), recursive }).map(|_| ())
    }

    pub fn rename(&self, host: &Host, from: &str, to: &str, opts: &RenameOpts) -> Result<()> {
        self.execute(host, &Request::Rename { from: from.into(), to: to.into(), opts: opts.clone() }).map(|_| ())
    }

    pub fn bookmark(&self, host: &Host, snapshot: &str, bookmark: &str) -> Result<()> {
        self.execute(host, &Request::Bookmark { snapshot: snapshot.into(), bookmark: bookmark.into() }).map(|_| ())
    }

    pub fn get(&self, host: &Host, dataset: &str, property: &str) -> Result<Option<String>> {
        let v = self.execute(host, &Request::Get { dataset: dataset.into(), property: property.into() })?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn recv_abort(&self, host: &Host, dataset: &str) -> Result<()> {
        self.execute(host, &Request::RecvAbort { dataset: dataset.into() }).map(|_| ())
    }

    pub fn estimate(&self, host: &Host, spec: &SendSpec) -> Result<Option<u64>> {
        let v = self.execute(host, &Request::Estimate { spec: spec.clone() })?;
        Ok(serde_json::from_value(v)?)
    }

    /// The pipeline stage that produces a send stream on `host`.
    pub fn send_stage(&self, host: &Host, spec: &SendSpec) -> Vec<String> {
        match self.rpc_target(host) {
            None => zfs::send_argv(&self.zfs, spec),
            Some(zsync) => {
                let mut argv = crate::util::split_words(zsync);
                argv.extend(["send".into(), "--zfs-path".into(), self.zfs.join(" ")]);
                argv.extend(zsync_send_flags(spec));
                argv
            }
        }
    }

    /// The pipeline stage that consumes a send stream on `host`.
    pub fn recv_stage(&self, host: &Host, spec: &RecvSpec) -> Vec<String> {
        match self.rpc_target(host) {
            None => zfs::recv_argv(&self.zfs, spec),
            Some(zsync) => {
                let mut argv = crate::util::split_words(zsync);
                argv.extend(["receive".into(), "--zfs-path".into(), self.zfs.join(" ")]);
                let flags = [
                    (spec.force, "--force"),
                    (spec.resumable, "--resumable"),
                    (spec.unmounted, "--unmounted"),
                    (spec.verbose, "--verbose"),
                ];
                argv.extend(flags.iter().filter(|(on, _)| *on).map(|(_, f)| f.to_string()));
                argv.push(spec.dataset.clone());
                argv
            }
        }
    }
}

/// `zsync send` flags equivalent to a [`SendSpec`].
pub fn zsync_send_flags(spec: &SendSpec) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(token) = &spec.resume_token {
        argv.extend(["--resume-token".into(), token.clone()]);
    } else {
        if let Some(from) = &spec.from {
            argv.extend(["--incremental".into(), from.clone()]);
        }
        argv.extend(["--snapshot".into(), spec.snapshot.clone()]);
    }
    let flags = [
        (spec.intermediary, "--intermediary"),
        (spec.replication, "--replication"),
        (spec.properties, "--properties"),
        (spec.compressed, "--compressed"),
        (spec.raw, "--raw"),
        (spec.large_block, "--large-block"),
        (spec.progress, "--progress"),
    ];
    argv.extend(flags.iter().filter(|(on, _)| *on).map(|(_, f)| f.to_string()));
    argv
}

/// Handle a `zsync rpc` invocation: parse the request, run it locally, return the envelope.
pub fn serve_rpc(zfs_words: Vec<String>, request_json: &str) -> Envelope {
    let backend = Backend { zfs: zfs_words, mode: Mode::Agentless };
    let local = Host::local(std::sync::Arc::new(Default::default()));
    let result = serde_json::from_str::<Request>(request_json)
        .map_err(|e| anyhow!("invalid rpc request: {e}"))
        .and_then(|req| backend.execute(&local, &req));
    match result {
        Ok(v) => Envelope::ok(v),
        Err(e) => Envelope::err(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_roundtrip() {
        let req = Request::List { query: ListQuery::snapshots_of("p/a", true) };
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.starts_with(r#"{"op":"list""#));
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), req);
        let req = Request::RecvAbort { dataset: "x".into() };
        assert_eq!(serde_json::to_string(&req).unwrap(), r#"{"op":"recv-abort","dataset":"x"}"#);
    }

    #[test]
    fn envelope_shapes() {
        let ok = serde_json::to_string(&Envelope::ok(Value::from(3))).unwrap();
        assert_eq!(ok, r#"{"version":1,"ok":true,"result":3}"#);
        let err = serde_json::to_string(&Envelope::err("boom".into())).unwrap();
        assert_eq!(err, r#"{"version":1,"ok":false,"error":{"message":"boom"}}"#);
    }

    #[test]
    fn rpc_rejects_bad_json() {
        let e = serve_rpc(vec!["zfs".into()], "{nope");
        assert!(!e.ok);
        assert!(e.error.unwrap().message.contains("invalid rpc request"));
        let v = serve_rpc(vec!["zfs".into()], r#"{"op":"version"}"#);
        assert!(v.ok);
    }

    #[test]
    fn send_flags_for_remote_zsync() {
        let spec = SendSpec { snapshot: "p/a@2".into(), from: Some("p/a@1".into()), intermediary: true, raw: true, ..Default::default() };
        assert_eq!(zsync_send_flags(&spec).join(" "), "--incremental p/a@1 --snapshot p/a@2 --intermediary --raw");
        let spec = SendSpec { resume_token: Some("t".into()), progress: true, ..Default::default() };
        assert_eq!(zsync_send_flags(&spec).join(" "), "--resume-token t --progress");
    }

    #[test]
    fn stages_by_mode() {
        let ssh = std::sync::Arc::new(crate::host::SshConfig::default());
        let remote = Host::new(Some("h"), ssh.clone());
        let local = Host::local(ssh);
        let spec = SendSpec { snapshot: "p/a@1".into(), ..Default::default() };
        let agentless = Backend::default();
        assert_eq!(agentless.send_stage(&remote, &spec), vec!["zfs", "send", "p/a@1"]);
        let rz = Backend { zfs: vec!["zfs".into()], mode: Mode::RemoteZsync { zsync: "/opt/zsync".into() } };
        assert_eq!(rz.send_stage(&remote, &spec), vec!["/opt/zsync", "send", "--zfs-path", "zfs", "--snapshot", "p/a@1"]);
        assert_eq!(rz.send_stage(&local, &spec), vec!["zfs", "send", "p/a@1"]);
        let r = RecvSpec { dataset: "b/a".into(), force: true, ..Default::default() };
        assert_eq!(rz.recv_stage(&remote, &r), vec!["/opt/zsync", "receive", "--zfs-path", "zfs", "--force", "b/a"]);
    }
}
