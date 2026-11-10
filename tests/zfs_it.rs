//! Integration tests against real ZFS on file-backed pools.
//!
//! Needs root, the zfs kernel module and `zpool`/`zfs` on PATH:
//!
//! ```sh
//! sudo -E env "PATH=$PATH" cargo test --features zfs-it --test zfs_it -- --test-threads=1
//! ```
//!
//! Optional:
//! * `ZSYNC_IT_SSH_HOST=localhost` — run the ssh tests (key-based root login to that host).
//! * `ZSYNC_IT_REMOTE_ZSYNC=1` — also test `--remote-zsync` (zsync must be on the remote PATH).
#![cfg(feature = "zfs-it")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn sh(cmd: &str) -> String {
    let out = Command::new("sh").args(["-c", cmd]).output().unwrap();
    assert!(out.status.success(), "`{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// A pool on a sparse file, destroyed on drop.
struct Pool {
    name: String,
    _dir: tempfile::TempDir,
}

impl Pool {
    fn new(tag: &str) -> Pool {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("pool.img");
        let name = format!("zsit{}{}{}", tag, std::process::id() % 10000, COUNTER.fetch_add(1, Ordering::SeqCst));
        sh(&format!("truncate -s 512M {}", img.display()));
        // atime=off: reading files on a mounted destination (as these tests do) would otherwise
        // count as a modification and make the next incremental receive fail without -F.
        sh(&format!("zpool create -f -O atime=off -O mountpoint=/{name} {name} {}", img.display()));
        Pool { name, _dir: dir }
    }
    fn ds(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{rel}", self.name)
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let _ = Command::new("zpool").args(["destroy", "-f", &self.name]).status();
    }
}

fn zsync(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zsync")).args(args).env("HOME", "/nonexistent-zsync-home").stdin(Stdio::null()).output().unwrap()
}

fn ok(args: &[&str]) -> String {
    let out = zsync(args);
    assert!(
        out.status.success(),
        "zsync {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn snaps(ds: &str) -> Vec<String> {
    let out = Command::new("zfs").args(["list", "-H", "-o", "name", "-t", "snapshot", "-s", "createtxg", "-d", "1", ds]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().lines().map(|l| l.split_once('@').unwrap().1.to_string()).collect()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn write_file(ds: &str, name: &str, content: &str) {
    let mp = sh(&format!("zfs get -H -o value mountpoint {ds}")).trim().to_string();
    std::fs::write(Path::new(&mp).join(name), content).unwrap();
}

fn read_file(ds: &str, name: &str) -> String {
    let mp = sh(&format!("zfs get -H -o value mountpoint {ds}")).trim().to_string();
    std::fs::read_to_string(PathBuf::from(mp).join(name)).unwrap()
}

fn prop(ds: &str, p: &str) -> String {
    sh(&format!("zfs get -H -p -o value {p} {ds}")).trim().to_string()
}

fn ssh_host() -> Option<String> {
    std::env::var("ZSYNC_IT_SSH_HOST").ok().filter(|h| !h.is_empty())
}

#[test]
fn full_then_incremental_with_data() {
    let src = Pool::new("s");
    let dst = Pool::new("d");
    sh(&format!("zfs create {}", src.ds("data")));
    write_file(&src.ds("data"), "a.txt", "one");
    ok(&["snapshot", &src.ds("data")]);
    ok(&["push", &src.ds("data"), &dst.name]);
    assert_eq!(read_file(&dst.ds("data"), "a.txt"), "one");

    write_file(&src.ds("data"), "a.txt", "two");
    ok(&["snapshot", &src.ds("data"), "hourly"]);
    ok(&["snapshot", &src.ds("data"), "hourly"]);
    ok(&["push", &src.ds("data"), &dst.name]);
    assert_eq!(read_file(&dst.ds("data"), "a.txt"), "two");
    assert_eq!(snaps(&dst.ds("data")), snaps(&src.ds("data")));
    let status = ok(&["status", &src.ds("data"), &dst.name]);
    assert!(status.lines().nth(1).unwrap().contains("true"), "{status}");
}

#[test]
fn latest_diverged_and_force() {
    let src = Pool::new("s");
    let dst = Pool::new("d");
    sh(&format!("zfs create {}", src.ds("a")));
    for n in 1..=3 {
        sh(&format!("zfs snapshot {}@{n}", src.ds("a")));
    }
    ok(&["push", "-L", &src.ds("a"), &dst.name]);
    assert_eq!(snaps(&dst.ds("a")), s(&["3"]));

    // modify destination -> receive refuses without -F
    sh(&format!("zfs snapshot {}@4", src.ds("a")));
    write_file(&dst.ds("a"), "junk", "x");
    let out = zsync(&["push", &src.ds("a"), &dst.name]);
    assert!(!out.status.success());
    ok(&["push", "-F", &src.ds("a"), &dst.name]);
    assert_eq!(snaps(&dst.ds("a")), s(&["3", "4"]));

    // diverged: rename fallback
    sh(&format!("zfs create {}", src.ds("b")));
    sh(&format!("zfs snapshot {}@x", src.ds("b")));
    sh(&format!("zfs create {}", dst.ds("b")));
    sh(&format!("zfs snapshot {}@other", dst.ds("b")));
    let out = zsync(&["push", &src.ds("b"), &dst.name]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not in common"));
    ok(&["push", "-e", &src.ds("b"), &dst.name]);
    assert_eq!(snaps(&dst.ds("b")), s(&["x"]));
    assert!(sh("zfs list -H -o name").lines().any(|l| l.starts_with(&format!("{}-renamed-", dst.ds("b")))));
}

#[test]
fn zvol_compressed_and_large_block() {
    let src = Pool::new("s");
    let dst = Pool::new("d");
    sh(&format!("zfs create -V 32M -o compression=lz4 {}", src.ds("vol")));
    sh(&format!("zfs snapshot {}@1", src.ds("vol")));
    ok(&["push", "-t", "volume", "-m", "--large-block", &src.ds("*"), &dst.name]);
    assert_eq!(prop(&dst.ds("vol"), "type"), "volume");
    assert_eq!(snaps(&dst.ds("vol")), s(&["1"]));
}

#[test]
fn encrypted_raw_send() {
    let src = Pool::new("s");
    let dst = Pool::new("d");
    let key = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(key.path(), "correct horse battery staple").unwrap();
    sh(&format!("zfs create -o encryption=on -o keyformat=passphrase -o keylocation=file://{} {}", key.path().display(), src.ds("secret")));
    write_file(&src.ds("secret"), "s.txt", "hidden");
    sh(&format!("zfs snapshot {}@1", src.ds("secret")));
    ok(&["push", "-W", &src.ds("secret"), &dst.name]);
    assert_eq!(prop(&dst.ds("secret"), "encryption"), "aes-256-gcm");
    assert_eq!(prop(&dst.ds("secret"), "keystatus"), "unavailable");
    sh(&format!("zfs snapshot {}@2", src.ds("secret")));
    ok(&["push", "-W", &src.ds("secret"), &dst.name]);
    assert_eq!(snaps(&dst.ds("secret")), s(&["1", "2"]));
}

#[test]
fn interrupted_transfer_resumes() {
    let src = Pool::new("s");
    let dst = Pool::new("d");
    sh(&format!("zfs create -o compression=off {}", src.ds("big")));
    sh(&format!("zfs snapshot {}@1", src.ds("big")));
    ok(&["push", &src.ds("big"), &dst.name]);
    let mp = prop(&src.ds("big"), "mountpoint");
    sh(&format!("head -c 24000000 /dev/urandom > {mp}/blob"));
    sh(&format!("zfs snapshot {}@2", src.ds("big")));

    // Throttle, kill mid-stream, then expect a resume token on the destination.
    let mut child = Command::new(env!("CARGO_BIN_EXE_zsync"))
        .args(["push", "--bwlimit", "4M", &src.ds("big"), &dst.name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    child.kill().unwrap();
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(20);
    while prop(&dst.ds("big"), "receive_resume_token") == "-" {
        assert!(Instant::now() < deadline, "no resume token appeared");
        std::thread::sleep(Duration::from_millis(200));
    }
    let j: serde_json::Value = serde_json::from_str(&ok(&["status", "-f", "json", &src.ds("big"), &dst.name])).unwrap();
    assert_eq!(j["result"]["results"][0]["resumePending"], true);
    let j: serde_json::Value = serde_json::from_str(&ok(&["push", "-f", "json", &src.ds("big"), &dst.name])).unwrap();
    assert_eq!(j["result"]["results"][0]["resumed"], true);
    assert_eq!(snaps(&dst.ds("big")), s(&["1", "2"]));
    assert_eq!(prop(&dst.ds("big"), "receive_resume_token"), "-");
}

#[test]
fn snapshot_rotate_prune_bookmark() {
    let src = Pool::new("s");
    let dst = Pool::new("d");
    sh(&format!("zfs create {}", src.ds("a")));
    for _ in 0..4 {
        ok(&["snapshot", &src.ds("a"), "hourly"]);
        std::thread::sleep(Duration::from_millis(5));
    }
    ok(&["push", "--bookmark", &src.ds("a"), &dst.name]);
    let base = snaps(&src.ds("a")).last().unwrap().clone();
    ok(&["rotate", &src.ds("a"), "hourly", "2"]);
    assert_eq!(snaps(&src.ds("a")).len(), 2);
    ok(&["prune", &src.ds("a"), &dst.name]);
    assert_eq!(snaps(&dst.ds("a")).len(), 2);

    // Remove every common snapshot from the source; the bookmark carries the incremental.
    ok(&["snapshot", &src.ds("a"), "hourly"]);
    sh(&format!("zfs destroy {}@{base}", src.ds("a")));
    for snap in snaps(&src.ds("a")).iter().filter(|n| **n != *snaps(&src.ds("a")).last().unwrap()) {
        sh(&format!("zfs destroy {}@{snap}", src.ds("a")));
    }
    let j: serde_json::Value = serde_json::from_str(&ok(&["push", "-f", "json", &src.ds("a"), &dst.name])).unwrap();
    assert_eq!(j["result"]["results"][0]["from"], format!("{}#{base}", src.ds("a")));
}

#[test]
fn over_ssh_relay_direct_and_tcp() {
    let Some(host) = ssh_host() else {
        eprintln!("ZSYNC_IT_SSH_HOST not set; skipping");
        return;
    };
    let src = Pool::new("s");
    let dst = Pool::new("d");
    for d in ["a", "b", "c", "d"] {
        sh(&format!("zfs create {}", src.ds(d)));
        write_file(&src.ds(d), "f", d);
        sh(&format!("zfs snapshot {}@1", src.ds(d)));
    }
    // remote source and destination (both the same machine via ssh), relayed
    ok(&["push", "-S", &host, &src.ds("a"), &dst.name, &host]);
    assert_eq!(read_file(&dst.ds("a"), "f"), "a");
    // direct: source ssh's to the destination
    ok(&["push", "-S", &host, "--transport", "direct", &src.ds("b"), &dst.name, &host]);
    assert_eq!(read_file(&dst.ds("b"), "f"), "b");
    // tcp with nc and zstd, with progress
    ok(&[
        "push",
        "-S",
        &host,
        "--transport",
        "tcp",
        "--tcp-host",
        "127.0.0.1",
        "--tcp-port",
        "19191",
        "--compress",
        "zstd",
        "--progress",
        &src.ds("c"),
        &dst.name,
        &host,
    ]);
    assert_eq!(read_file(&dst.ds("c"), "f"), "c");
    // pull to local
    ok(&["pull", &src.ds("d"), &host, &dst.name]);
    assert_eq!(read_file(&dst.ds("d"), "f"), "d");

    if std::env::var("ZSYNC_IT_REMOTE_ZSYNC").is_ok() {
        sh(&format!("zfs snapshot {}@2", src.ds("a")));
        ok(&["push", "--remote-zsync", "-S", &host, &src.ds("a"), &dst.name, &host]);
        assert_eq!(snaps(&dst.ds("a")), s(&["1", "2"]));
        sh(&format!("zfs snapshot {}@3", src.ds("a")));
        ok(&[
            "push",
            "--remote-zsync",
            "-S",
            &host,
            "--transport",
            "tcp",
            "--tcp-tool",
            "zsync",
            "--tcp-host",
            "127.0.0.1",
            "--tcp-port",
            "19192",
            &src.ds("a"),
            &dst.name,
            &host,
        ]);
        assert_eq!(snaps(&dst.ds("a")), s(&["1", "2", "3"]));
    }
}
