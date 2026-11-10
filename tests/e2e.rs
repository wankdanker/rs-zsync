//! End-to-end tests: the real `zsync` binary against the fake `zfs` and `ssh` (see
//! `src/bin/fake_zfs.rs`). Run with `cargo test --features fakes`.
#![cfg(feature = "fakes")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fake-zfs"), bin.join("zfs")).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fake-ssh"), bin.join("ssh")).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_zsync"), bin.join("zsync")).unwrap();
        Env { dir }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn path_var(&self) -> String {
        format!("{}:{}", self.p("bin").display(), std::env::var("PATH").unwrap_or_default())
    }

    fn cmd(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("PATH", self.path_var())
            .env("FAKEZFS_ROOT", self.p("state"))
            .env("FAKEZFS_STATE", self.p("state/local.json"))
            .env("HOME", self.dir.path())
            .env("ZSYNC_SYSCONFDIR", self.p("etc"))
            .env_remove("DEBUG")
            .env_remove("CLYOPS_FDS")
            .stdin(Stdio::null());
        c
    }

    /// Run the fake zfs as `host` ("local" for this machine).
    fn zfs(&self, host: &str, args: &[&str]) -> String {
        let out = self
            .cmd(env!("CARGO_BIN_EXE_fake-zfs"))
            .env("FAKEZFS_STATE", self.p(&format!("state/{host}.json")))
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "zfs {args:?} on {host}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn create(&self, host: &str, datasets: &[&str]) {
        for d in datasets {
            self.zfs(host, &["create", "-p", d]);
        }
    }

    fn snap(&self, host: &str, full: &str) {
        self.zfs(host, &["snapshot", full]);
    }

    /// Short snapshot names of `dataset` on `host`, oldest first (empty when missing).
    fn snaps(&self, host: &str, dataset: &str) -> Vec<String> {
        let out = self
            .cmd(env!("CARGO_BIN_EXE_fake-zfs"))
            .env("FAKEZFS_STATE", self.p(&format!("state/{host}.json")))
            .args(["list", "-H", "-o", "name", "-t", "snapshot", "-s", "createtxg", "-d", "1", dataset])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().lines().map(|l| l.split_once('@').unwrap().1.to_string()).collect()
    }

    fn bookmarks(&self, host: &str, dataset: &str) -> Vec<String> {
        self.zfs(host, &["list", "-H", "-o", "name", "-t", "bookmark", "-d", "1", dataset])
            .lines()
            .map(|l| l.split_once('#').unwrap().1.to_string())
            .collect()
    }

    fn datasets(&self, host: &str) -> Vec<String> {
        self.zfs(host, &["list", "-H", "-o", "name"]).lines().map(String::from).collect()
    }

    fn exists(&self, host: &str, dataset: &str) -> bool {
        self.datasets(host).iter().any(|d| d == dataset)
    }

    fn zsync(&self, args: &[&str]) -> Output {
        self.cmd(env!("CARGO_BIN_EXE_zsync")).args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.zsync(args);
        assert!(
            out.status.success(),
            "zsync {args:?} failed ({:?}):\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn fails(&self, args: &[&str], code: i32) -> (String, String) {
        let out = self.zsync(args);
        assert_eq!(
            out.status.code(),
            Some(code),
            "zsync {args:?}: stdout {} stderr {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut a = args.to_vec();
        a.extend(["--format", "json"]);
        serde_json::from_str(&self.ok(&a)).unwrap()
    }

    /// The README layout: pool1/{sub1,sub1/swap,sub2}, pool2/sub3 locally; pool3 on "backup".
    fn readme_layout(&self) {
        self.create("local", &["pool1/sub1/swap", "pool1/sub2", "pool2/sub3"]);
        self.create("backup", &["pool3"]);
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn list_globs_and_excludes() {
    let e = Env::new();
    e.readme_layout();
    assert_eq!(e.ok(&["list"]), "pool1\npool1/sub1\npool1/sub1/swap\npool1/sub2\npool2\npool2/sub3\n");
    assert_eq!(e.ok(&["list", "pool1/**"]), "pool1/sub1\npool1/sub1/swap\npool1/sub2\n");
    assert_eq!(e.ok(&["list", "--exclude", "*/*2", "pool1/*"]), "pool1/sub1\n");
    assert_eq!(e.ok(&["list", "-g", "pool2,pool1"]), "pool1\npool2\n");
    assert_eq!(e.ok(&["list", "-s", "pool1", "-R", "**/swap"]), "pool1/sub1/swap\n");
    let j = e.json(&["list", "pool2/*"]);
    assert_eq!(j["ok"], true);
    assert_eq!(j["result"][0]["name"], "pool2/sub3");
    assert_eq!(j["result"][0]["type"], "filesystem");
    // remote listing
    assert_eq!(e.ok(&["list", "-S", "backup"]), "pool3\n");
}

#[test]
fn status_push_incremental_roundtrip() {
    let e = Env::new();
    e.readme_layout();
    e.snap("local", "pool1/sub1@a");
    e.snap("local", "pool1/sub1@b");
    let out = e.ok(&["status", "--exclude", "*/*2", "pool1/*", "pool3", "backup"]);
    assert_eq!(
        out,
        "source      destination  destination-host  up-to-date  latest-snap-shot\npool1/sub1  pool3/sub1   backup            false\n"
    );

    // First push: full replication stream carries every snapshot.
    assert_eq!(e.ok(&["push", "-x", "*/*2", "pool1/*", "pool3", "backup"]), "done\n");
    assert_eq!(e.snaps("backup", "pool3/sub1"), s(&["a", "b"]));
    let out = e.ok(&["status", "-x", "*/*2", "pool1/*", "pool3", "backup"]);
    assert!(out.contains("pool1/sub1  pool3/sub1   backup            true        pool1/sub1@b"), "{out}");

    // Incremental with intermediates.
    e.snap("local", "pool1/sub1@c");
    e.snap("local", "pool1/sub1@d");
    let j = e.json(&["push", "-x", "*/*2", "pool1/*", "pool3", "backup"]);
    assert_eq!(j["result"]["results"][0]["action"], "incremental");
    assert_eq!(j["result"]["results"][0]["from"], "pool1/sub1@b");
    assert_eq!(e.snaps("backup", "pool3/sub1"), s(&["a", "b", "c", "d"]));

    // Up to date: nothing to do.
    let j = e.json(&["push", "-x", "*/*2", "pool1/*", "pool3", "backup"]);
    assert_eq!(j["result"]["results"][0]["action"], "up-to-date");
}

#[test]
fn latest_only_sends_newest() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    for n in ["1", "2", "3"] {
        e.snap("local", &format!("tank/a@{n}"));
    }
    e.ok(&["push", "--latest", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["3"]));
    for n in ["4", "5"] {
        e.snap("local", &format!("tank/a@{n}"));
    }
    e.ok(&["push", "-L", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["3", "5"]));
}

#[test]
fn non_sequential_snapshot_sets() {
    let e = Env::new();
    e.create("local", &["tank/vm"]);
    e.create("backup", &["bk"]);
    for n in ["1", "2", "3"] {
        e.snap("local", &format!("tank/vm@{n}"));
    }
    e.ok(&["push", "tank/vm", "bk", "backup"]);
    // Source rotates away 1 and 2 and takes more; destination keeps its history.
    e.zfs("local", &["destroy", "tank/vm@1"]);
    e.zfs("local", &["destroy", "tank/vm@2"]);
    e.snap("local", "tank/vm@4");
    e.snap("local", "tank/vm@5");
    let j = e.json(&["status", "tank/vm", "bk", "backup"]);
    assert_eq!(j["result"]["results"][0]["latest"], "tank/vm@3");
    assert_eq!(j["result"]["results"][0]["diff"]["missing"], serde_json::json!(["4", "5"]));
    e.ok(&["push", "tank/vm", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/vm"), s(&["1", "2", "3", "4", "5"]));
}

#[test]
fn diverged_destination_and_fallbacks() {
    let e = Env::new();
    e.create("local", &["tank/a", "tank/b"]);
    e.create("backup", &["bk/a", "bk/b"]);
    e.snap("local", "tank/a@s1");
    e.snap("local", "tank/b@s1");
    e.snap("backup", "bk/a@other");

    let (_, err) = e.fails(&["push", "tank/*", "bk", "backup"], 1);
    assert!(err.contains("not in common with source"), "{err}");

    // --continue: tank/b still syncs (into an existing, empty dataset this needs -F), exit 3.
    let (out, err) = e.fails(&["push", "-c", "-F", "tank/*", "bk", "backup"], 3);
    assert_eq!(out, "done\n");
    assert!(err.contains("1 dataset(s) failed"), "{err}");
    assert_eq!(e.snaps("backup", "bk/b"), s(&["s1"]));

    // Rename the diverged destination out of the way.
    e.ok(&["push", "--fallback-rename-destination", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["s1"]));
    let renamed: Vec<String> = e.datasets("backup").into_iter().filter(|d| d.starts_with("bk/a-renamed-")).collect();
    assert_eq!(renamed.len(), 1);
    assert_eq!(e.snaps("backup", &renamed[0]), s(&["other"]));

    // Destroy fallback.
    e.create("local", &["tank/c"]);
    e.snap("local", "tank/c@s1");
    e.create("backup", &["bk/c"]);
    e.snap("backup", "bk/c@junk");
    e.ok(&["push", "-X", "tank/c", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/c"), s(&["s1"]));
}

#[test]
fn modified_destination_needs_force() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    e.snap("local", "tank/a@1");
    e.ok(&["push", "tank/a", "bk", "backup"]);
    e.snap("local", "tank/a@2");
    e.zfs("backup", &["fake-write", "bk/a"]);
    let (_, err) = e.fails(&["push", "tank/a", "bk", "backup"], 1);
    assert!(err.contains("receive:") && err.contains("modified"), "{err}");
    e.ok(&["push", "-F", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1", "2"]));

    // A destination-only snapshot after the common one also needs -F (rollback).
    e.snap("backup", "bk/a@local-only");
    e.snap("local", "tank/a@3");
    let j = e.json(&["status", "tank/a", "bk", "backup"]);
    assert_eq!(j["result"]["results"][0]["diff"]["destinationExtra"], serde_json::json!(["local-only"]));
    e.fails(&["push", "tank/a", "bk", "backup"], 1);
    e.ok(&["push", "--force", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1", "2", "3"]));
}

#[test]
fn destination_name_mapping() {
    let e = Env::new();
    e.create("local", &["tank/vms/web/disk0"]);
    e.create("backup", &["bk/vms/web", "bk2"]);
    e.snap("local", "tank/vms/web/disk0@1");
    e.ok(&["push", "-n", "1", "tank/vms/web/disk0", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/vms/web/disk0"), s(&["1"]));
    e.ok(&["push", "-N", "1", "tank/vms/web/disk0", "bk2", "backup"]);
    assert_eq!(e.snaps("backup", "bk2/disk0"), s(&["1"]));
    e.create("backup", &["full"]);
    e.zfs("backup", &["create", "-p", "full/tank/vms/web"]);
    e.ok(&["push", "-n", "0", "-N", "0", "tank/vms/web/disk0", "full", "backup"]);
    assert_eq!(e.snaps("backup", "full/tank/vms/web/disk0"), s(&["1"]));
}

#[test]
fn local_pull_and_remote_to_remote() {
    let e = Env::new();
    e.create("local", &["tank/a", "copies"]);
    e.snap("local", "tank/a@1");
    // local -> local
    e.ok(&["push", "tank/a", "copies"]);
    assert_eq!(e.snaps("local", "copies/a"), s(&["1"]));

    // pull: remote source -> local
    e.create("nas", &["data/photos"]);
    e.snap("nas", "data/photos@p1");
    e.ok(&["pull", "data/*", "nas", "copies"]);
    assert_eq!(e.snaps("local", "copies/photos"), s(&["p1"]));

    // remote -> remote, relayed through here
    e.create("offsite", &["vault"]);
    e.ok(&["push", "-S", "nas", "data/*", "vault", "offsite"]);
    assert_eq!(e.snaps("offsite", "vault/photos"), s(&["p1"]));
}

#[test]
fn transports_direct_and_tcp() {
    let e = Env::new();
    e.create("nas", &["data/a", "data/b", "data/c"]);
    e.create("offsite", &["vault"]);
    for d in ["a", "b", "c"] {
        e.snap("nas", &format!("data/{d}@1"));
    }
    e.ok(&["push", "-S", "nas", "--transport", "direct", "data/a", "vault", "offsite"]);
    assert_eq!(e.snaps("offsite", "vault/a"), s(&["1"]));

    let has_nc = Command::new("sh").args(["-c", "command -v nc"]).output().unwrap().status.success();
    let port = (21000 + std::process::id() % 20000).to_string();
    if has_nc {
        e.ok(&["push", "-S", "nas", "--transport", "tcp", "--tcp-host", "127.0.0.1", "--tcp-port", &port, "data/b", "vault", "offsite"]);
        assert_eq!(e.snaps("offsite", "vault/b"), s(&["1"]));
    }
    let port2 = (22000 + std::process::id() % 20000).to_string();
    e.ok(&[
        "push",
        "-S",
        "nas",
        "--transport",
        "tcp",
        "--tcp-tool",
        "zsync",
        "--tcp-host",
        "127.0.0.1",
        "--tcp-port",
        &port2,
        "data/c",
        "vault",
        "offsite",
    ]);
    assert_eq!(e.snaps("offsite", "vault/c"), s(&["1"]));

    // direct to a local destination is refused with a clear message
    e.create("local", &["here"]);
    let (_, err) = e.fails(&["push", "-S", "nas", "--transport", "direct", "data/a", "here"], 1);
    assert!(err.contains("direct transport needs a remote destination"), "{err}");
}

#[test]
fn filters_compression_and_bwlimit() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    e.snap("local", "tank/a@1");
    e.ok(&["push", "--compress", "gzip", "--send-pipe", "cat", "--receive-pipe", "cat", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1"]));

    // A failing receive-side filter is reported with its label.
    e.snap("local", "tank/a@2");
    let (_, err) = e.fails(&["push", "--receive-pipe", "exit 7", "tank/a", "bk", "backup"], 1);
    assert!(err.contains("receive-pipe-1 exited 7"), "{err}");

    // bwlimit on the relay: 64 KiB at 128 KiB/s takes about half a second.
    let start = std::time::Instant::now();
    e.ok(&["push", "--bwlimit", "128K", "tank/a", "bk", "backup"]);
    assert!(start.elapsed() >= std::time::Duration::from_millis(400), "{:?}", start.elapsed());
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1", "2"]));
}

#[test]
fn resumable_receive() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    e.snap("local", "tank/a@1");
    e.ok(&["push", "tank/a", "bk", "backup"]);
    e.snap("local", "tank/a@2");
    e.snap("local", "tank/a@3");

    let fail = e.p("fail-once");
    std::fs::write(&fail, "").unwrap();
    let mut c = e.cmd(env!("CARGO_BIN_EXE_zsync"));
    c.env("FAKEZFS_FAIL_ONCE_FILE", &fail).env("FAKEZFS_FAIL_AFTER_BYTES", "50000").args(["push", "tank/a", "bk", "backup"]);
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("transfer to bk/a failed"));
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1"]));

    let j = e.json(&["status", "tank/a", "bk", "backup"]);
    assert_eq!(j["result"]["results"][0]["resumePending"], true);

    let j = e.json(&["push", "tank/a", "bk", "backup"]);
    assert_eq!(j["result"]["results"][0]["resumed"], true);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1", "2", "3"]));

    // --retries resumes within one run.
    e.snap("local", "tank/a@4");
    std::fs::write(&fail, "").unwrap();
    let mut c = e.cmd(env!("CARGO_BIN_EXE_zsync"));
    c.env("FAKEZFS_FAIL_ONCE_FILE", &fail).env("FAKEZFS_FAIL_AFTER_BYTES", "1000").args([
        "push",
        "--retries",
        "1",
        "tank/a",
        "bk",
        "backup",
    ]);
    let out = c.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("retry 1/1"));
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1", "2", "3", "4"]));

    // --abort-resume discards the partial state and sends again from scratch.
    e.snap("local", "tank/a@5");
    std::fs::write(&fail, "").unwrap();
    let mut c = e.cmd(env!("CARGO_BIN_EXE_zsync"));
    c.env("FAKEZFS_FAIL_ONCE_FILE", &fail).args(["push", "tank/a", "bk", "backup"]);
    assert_eq!(c.output().unwrap().status.code(), Some(1));
    let j = e.json(&["push", "--abort-resume", "tank/a", "bk", "backup"]);
    assert_eq!(j["result"]["results"][0]["resumed"], false);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1", "2", "3", "4", "5"]));
}

#[test]
fn interrupted_full_send_without_resume_leaves_nothing() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    e.snap("local", "tank/a@1");
    let fail = e.p("fail-once");
    std::fs::write(&fail, "").unwrap();
    let mut c = e.cmd(env!("CARGO_BIN_EXE_zsync"));
    // A full first send is a replication stream, so it is never resumable.
    c.env("FAKEZFS_FAIL_ONCE_FILE", &fail).args(["push", "tank/a", "bk", "backup"]);
    assert_eq!(c.output().unwrap().status.code(), Some(1));
    assert!(!e.exists("backup", "bk/a"));
    e.ok(&["push", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["1"]));
}

#[test]
fn remote_zsync_mode() {
    let e = Env::new();
    e.create("nas", &["data/a"]);
    e.create("offsite", &["vault"]);
    e.snap("nas", "data/a@1");
    let log = e.p("zfs.log");
    let mut c = e.cmd(env!("CARGO_BIN_EXE_zsync"));
    c.env("FAKEZFS_LOG", &log).args(["push", "--remote-zsync", "-S", "nas", "data/*", "vault", "offsite"]);
    let out = c.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(e.snaps("offsite", "vault/a"), s(&["1"]));
    let j = e.json(&["status", "--remote-zsync", "-S", "nas", "data/*", "vault", "offsite"]);
    assert_eq!(j["result"]["results"][0]["upToDate"], true);
    assert_eq!(e.ok(&["list", "--remote-zsync", "-S", "nas"]), "data\ndata/a\n");
    // rpc errors come back as messages
    let (_, err) = e.fails(&["list", "--remote-zsync", "-S", "nas", "-s", "missing"], 1);
    assert!(err.contains("dataset does not exist"), "{err}");
}

#[test]
fn snapshot_naming_and_atomic() {
    let e = Env::new();
    e.create("local", &["tank/a", "tank/b"]);
    e.ok(&["snapshot", "tank/*", "daily"]);
    e.ok(&["snapshot", "-p", "manual", "tank/*"]);
    e.ok(&["snapshot", "tank/a", "", "yyyy"]);
    let a = e.snaps("local", "tank/a");
    assert_eq!(a.len(), 3);
    assert!(a[0].starts_with("zsync-daily-") && a[0].len() == "zsync-daily-".len() + 17, "{a:?}");
    assert_eq!(a[1], "manual");
    assert!(a[2].starts_with("zsync-2") && a[2].len() == "zsync-".len() + 4, "{a:?}");

    let out = e.ok(&["snapshot", "--dry-run", "--prefix", "auto", "--tag", "x", "tank/*"]);
    assert_eq!(out.lines().count(), 2);
    assert!(out.starts_with("tank/a@auto-x-"));
    assert_eq!(e.snaps("local", "tank/a").len(), 3);

    // atomic: one zfs snapshot call -> same txg for every dataset
    e.ok(&["snapshot", "-a", "-p", "together", "tank/*"]);
    let txgs: Vec<String> = e
        .zfs("local", &["list", "-H", "-o", "name,createtxg", "-t", "snapshot"])
        .lines()
        .filter(|l| l.contains("@together"))
        .map(|l| l.split('\t').nth(1).unwrap().to_string())
        .collect();
    assert_eq!(txgs.len(), 2);
    assert_eq!(txgs[0], txgs[1]);

    // duplicate name fails; --continue reports
    let (_, err) = e.fails(&["snapshot", "-c", "-p", "manual", "tank/*"], 3);
    assert!(err.contains("2 dataset(s) failed"), "{err}");
}

#[test]
fn rotate_keeps_newest_and_preserves_base() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    for n in 1..=5 {
        e.snap("local", &format!("tank/a@zsync-hourly-{n}"));
    }
    e.snap("local", "tank/a@zsync-daily-1");
    e.snap("local", "tank/a@manual");

    let (_, err) = e.fails(&["rotate", "tank/a", "hourly"], 1);
    assert!(err.contains("You must specify a value for keep."), "{err}");

    let out = e.ok(&["rotate", "--dry-run", "tank/a", "hourly", "3"]);
    assert_eq!(out, "local:tank/a@zsync-hourly-1\nlocal:tank/a@zsync-hourly-2\n");
    e.ok(&["rotate", "tank/a", "hourly", "3"]);
    assert_eq!(e.snaps("local", "tank/a"), s(&["zsync-hourly-3", "zsync-hourly-4", "zsync-hourly-5", "zsync-daily-1", "manual"]));

    // With a destination, the newest common snapshot survives (and counts toward keep).
    e.create("local", &["tank/b"]);
    for n in 1..=3 {
        e.snap("local", &format!("tank/b@zsync-hourly-{n}"));
    }
    e.ok(&["push", "tank/b", "bk", "backup"]);
    for n in 4..=7 {
        e.snap("local", &format!("tank/b@zsync-hourly-{n}"));
    }
    e.ok(&["rotate", "tank/b", "hourly", "2", "bk", "backup"]);
    assert_eq!(e.snaps("local", "tank/b"), s(&["zsync-hourly-3", "zsync-hourly-7"]));

    // --snapshot removes exactly one, but not the incremental base.
    let out = e.zsync(&["rotate", "-p", "zsync-hourly-3", "-d", "bk", "-D", "backup", "tank/b"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not destroying it"));
    assert!(e.snaps("local", "tank/b").contains(&"zsync-hourly-3".to_string()));
    e.ok(&["rotate", "-p", "zsync-daily-1", "tank/a"]);
    assert!(!e.snaps("local", "tank/a").contains(&"zsync-daily-1".to_string()));
}

#[test]
fn rotate_destination_side_and_prune() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    for n in 1..=4 {
        e.snap("local", &format!("tank/a@zsync-{n}"));
    }
    e.ok(&["push", "tank/a", "bk", "backup"]);
    e.zfs("local", &["destroy", "tank/a@zsync-1"]);
    e.zfs("local", &["destroy", "tank/a@zsync-2"]);
    e.snap("backup", "bk/a@keepme");

    // prune: destination snapshots gone from the source (zsync-named only)
    let out = e.ok(&["prune", "--dry-run", "tank/a", "bk", "backup"]);
    assert_eq!(out, "backup:bk/a@zsync-1\nbackup:bk/a@zsync-2\n");
    e.ok(&["prune", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["zsync-3", "zsync-4", "keepme"]));
    e.ok(&["prune", "--all", "tank/a", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["zsync-3", "zsync-4"]));

    // destination-side rotation keeps the newest N there, never the common base
    for n in 5..=7 {
        e.snap("local", &format!("tank/a@zsync-{n}"));
    }
    e.ok(&["push", "tank/a", "bk", "backup"]);
    e.ok(&["rotate", "--destination-side", "tank/a", "", "2", "bk", "backup"]);
    assert_eq!(e.snaps("backup", "bk/a"), s(&["zsync-6", "zsync-7"]));
    assert_eq!(e.snaps("local", "tank/a").len(), 5);

    // prune refuses a destination with nothing in common
    e.create("local", &["tank/z"]);
    e.snap("local", "tank/z@zsync-9");
    e.create("backup", &["bk/z"]);
    e.snap("backup", "bk/z@zsync-1");
    let out = e.zsync(&["prune", "tank/z", "bk", "backup"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no snapshot in common"));
    assert_eq!(e.snaps("backup", "bk/z"), s(&["zsync-1"]));
}

#[test]
fn bookmarks_keep_incremental_base() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    e.snap("local", "tank/a@zsync-1");
    e.ok(&["push", "--bookmark", "tank/a", "bk", "backup"]);
    assert_eq!(e.bookmarks("local", "tank/a"), s(&["zsync-1"]));
    e.snap("local", "tank/a@zsync-2");
    e.ok(&["push", "--bookmark", "tank/a", "bk", "backup"]);
    assert_eq!(e.bookmarks("local", "tank/a"), s(&["zsync-2"]));

    // Source loses every snapshot the destination has; the bookmark still works as a base.
    e.snap("local", "tank/a@zsync-3");
    e.zfs("local", &["destroy", "tank/a@zsync-1"]);
    e.zfs("local", &["destroy", "tank/a@zsync-2"]);
    let j = e.json(&["push", "tank/a", "bk", "backup"]);
    assert_eq!(j["result"]["results"][0]["from"], "tank/a#zsync-2");
    assert_eq!(e.snaps("backup", "bk/a"), s(&["zsync-1", "zsync-2", "zsync-3"]));
}

#[test]
fn presets_json_native_and_precedence() {
    let e = Env::new();
    e.create("local", &["pool1/sub1", "pool1/sub2", "pool1/swap"]);
    e.create("backup", &["pool3", "other"]);
    e.snap("local", "pool1/sub1@1");
    e.snap("local", "pool1/sub2@1");
    std::fs::write(
        e.p(".zsync.json"),
        r#"{"preset1": {"glob": "pool1/*", "exclude": "*/*swap,*/*2", "destination": "pool3", "destinationHost": "backup", "replication": true}}"#,
    )
    .unwrap();
    e.ok(&["push", "preset1"]);
    assert_eq!(e.snaps("backup", "pool3/sub1"), s(&["1"]));
    assert!(!e.exists("backup", "pool3/sub2"));
    let out = e.ok(&["status", "preset1"]);
    assert!(out.contains("pool1/sub1  pool3/sub1   backup            true"), "{out}");

    // explicit options beat the preset
    e.ok(&["push", "preset1", "-d", "other"]);
    assert_eq!(e.snaps("backup", "other/sub1"), s(&["1"]));

    // native config file with --config, including a global default
    std::fs::write(e.p("my.conf"), "exclude=*/*swap\nnightly:glob=pool1/*\nnightly:destination=pool3\nnightly:destination-host=backup\n")
        .unwrap();
    e.ok(&["push", "-C", e.p("my.conf").to_str().unwrap(), "nightly"]);
    assert_eq!(e.snaps("backup", "pool3/sub2"), s(&["1"]));

    // a missing --config is exit 2
    let (_, err) = e.fails(&["push", "-C", "/nonexistent.json", "preset1"], 2);
    assert!(err.contains("config file not found"), "{err}");

    // /etc layer
    std::fs::write(e.p("etc/zsync.conf"), "etcpreset:glob=pool1/sub1\n").unwrap();
    assert_eq!(e.ok(&["list", "etcpreset"]), "pool1/sub1\n");
}

#[test]
fn lock_file_contention() {
    let e = Env::new();
    e.create("local", &["tank"]);
    let lock = e.p("zsync.lock");
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock).unwrap();
    // SAFETY: test holds an exclusive lock on its own file.
    unsafe { libc_flock(std::os::unix::io::AsRawFd::as_raw_fd(&f)) };
    let (_, err) = e.fails(&["list", "-l", lock.to_str().unwrap(), "-w", "200"], 1);
    assert!(err.contains("held by another zsync"), "{err}");
    drop(f);
    e.ok(&["list", "-l", lock.to_str().unwrap(), "-w", "200"]);
}

unsafe fn libc_flock(fd: i32) {
    extern "C" {
        fn flock(fd: i32, op: i32) -> i32;
    }
    assert_eq!(flock(fd, 2 | 4), 0);
}

#[test]
fn ssh_options_are_passed() {
    let e = Env::new();
    e.create("nas", &["data"]);
    let log = e.p("ssh.log");
    let mut c = e.cmd(env!("CARGO_BIN_EXE_zsync"));
    c.env("FAKESSH_LOG", &log).args([
        "list",
        "-S",
        "nas",
        "-u",
        "backup",
        "-k",
        "/keys/id",
        "--ssh-port",
        "2222",
        "--ssh-opt",
        "Compression=no",
        "--ssh-opt",
        "ConnectTimeout=5",
    ]);
    assert!(c.output().unwrap().status.success());
    let line = std::fs::read_to_string(&log).unwrap();
    assert!(line.starts_with("-l backup -i /keys/id -p 2222 -o Compression=no -o ConnectTimeout=5 nas zfs list"), "{line}");

    let (_, err) = e.fails(&["list", "-S", "unreachable"], 1);
    assert!(err.contains("Could not resolve hostname"), "{err}");
}

#[test]
fn send_and_receive_commands() {
    let e = Env::new();
    e.create("local", &["tank/a", "copy"]);
    e.snap("local", "tank/a@1");
    e.snap("local", "tank/a@2");
    let script = "zsync send -R -s tank/a@1 | zsync receive copy/a && zsync send -I -i tank/a@1 -s tank/a@2 | zsync receive -d copy/a";
    let out = e.cmd("sh").args(["-c", script]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(e.snaps("local", "copy/a"), s(&["1", "2"]));

    // over TCP with the built-in listener/connector
    let port = 23000 + std::process::id() % 20000;
    let script = format!("zsync receive --listen 127.0.0.1:{port} copy/b & zsync send -s tank/a@2 --connect 127.0.0.1:{port}; wait $!");
    let out = e.cmd("sh").args(["-c", &script]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(e.snaps("local", "copy/b"), s(&["2"]));

    let (_, err) = e.fails(&["send"], 1);
    assert!(err.contains("snapshot to send is required"), "{err}");
}

#[test]
fn destroy_and_rename_commands() {
    let e = Env::new();
    e.create("nas", &["data/a/child", "data/b"]);
    e.ok(&["rename", "data/b", "data/c", "nas"]);
    assert!(e.exists("nas", "data/c"));
    e.ok(&["rename", "--parents", "-s", "data/c", "-n", "x/y/z", "-S", "nas"]);
    assert!(e.exists("nas", "x/y/z"));
    let (_, err) = e.fails(&["destroy", "data/a", "nas"], 1);
    assert!(err.contains("has children"), "{err}");
    e.ok(&["destroy", "-R", "data/a", "nas"]);
    assert!(!e.exists("nas", "data/a"));
    assert!(!e.exists("nas", "data/a/child"));
}

#[test]
fn progress_ndjson_on_clyops_role() {
    let e = Env::new();
    e.create("local", &["tank/a"]);
    e.create("backup", &["bk"]);
    e.snap("local", "tank/a@1");
    let file = e.p("progress.ndjson");
    let script = format!("zsync push tank/a bk backup 3>{}", file.display());
    let out = e.cmd("sh").env("CLYOPS_FDS", "progress:3").args(["-c", &script]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&file).unwrap();
    let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(last["done"], true);
    assert_eq!(last["dataset"], "bk/a");
    assert!(last["bytes"].as_u64().unwrap() >= 65536);
    assert!(last["total"].as_u64().unwrap() >= 65536);
}

#[test]
fn help_schema_and_errors() {
    let e = Env::new();
    let out = e.ok(&["--help-json-schema"]);
    let schema: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(schema["script"], "zsync");
    let out = e.ok(&["rotate", "--help"]);
    assert!(out.contains("--preserve-incremental"));
    let (_, err) = e.fails(&["push", "--nope"], 1);
    assert!(err.contains("Unknown option: --nope"));
    let (_, err) = e.fails(&["push", "tank/*"], 1);
    assert!(err.contains("a destination is required"), "{err}");
    let (out, _) = e.fails(&["push", "tank/*", "--format", "json"], 1);
    let j: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(j["ok"], false);
    let _ = Path::new("");
}
