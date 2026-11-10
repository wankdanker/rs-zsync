//! A stateful stand-in for `zfs`, for end-to-end tests without a ZFS kernel module.
//!
//! State lives in a JSON file (`$FAKEZFS_STATE`, default `$FAKEZFS_ROOT/local.json`). It models
//! what zsync relies on: datasets, snapshots with GUIDs and txgs, bookmarks, `zfs list` columns,
//! send streams (full, `-i`, `-I`, `-R`), receive rules (existing destinations, `-F` rollback,
//! "modified since most recent snapshot", "does not match incremental source"), resumable
//! receives (`-s`, `receive_resume_token`, `send -t`, `receive -A`) and `-nvP` estimates.
//!
//! Extra subcommands for tests: `create [-p] [-V size] ds`, `fake-write ds` (dirty the dataset),
//! `fake-dump` (print state). Fault injection: if `$FAKEZFS_FAIL_ONCE_FILE` exists, `send`
//! deletes it and dies after `$FAKEZFS_FAIL_AFTER_BYTES` payload bytes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

const MAGIC: &str = "FAKEZFS1";
const TRAILER: &[u8] = b"FAKEZFS-END\n";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SnapRec {
    name: String,
    guid: u64,
    createtxg: u64,
    size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Ds {
    kind: String,
    guid: u64,
    createtxg: u64,
    #[serde(default)]
    snaps: Vec<SnapRec>,
    #[serde(default)]
    bookmarks: Vec<SnapRec>,
    #[serde(default)]
    dirty: bool,
    #[serde(default)]
    resume_token: Option<String>,
    #[serde(default)]
    partial: bool,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct State {
    txg: u64,
    datasets: BTreeMap<String, Ds>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ChildStream {
    relname: String,
    kind: String,
    snaps: Vec<SnapRec>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Header {
    magic: String,
    full: bool,
    replication: bool,
    source: String,
    kind: String,
    #[serde(default)]
    from_guid: Option<u64>,
    snaps: Vec<SnapRec>,
    #[serde(default)]
    children: Vec<ChildStream>,
    #[serde(default)]
    resume_offset: u64,
}

impl Header {
    fn total(&self) -> u64 {
        self.snaps.iter().map(|s| s.size).sum::<u64>() + self.children.iter().flat_map(|c| &c.snaps).map(|s| s.size).sum::<u64>()
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("FAKEZFS_STATE") {
        return PathBuf::from(p);
    }
    let root = std::env::var("FAKEZFS_ROOT").unwrap_or_else(|_| die("fake-zfs: set FAKEZFS_STATE or FAKEZFS_ROOT"));
    PathBuf::from(root).join("local.json")
}

struct Locked {
    _f: std::fs::File,
}

fn lock() -> Locked {
    let p = state_path().with_extension("lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&p)
        .unwrap_or_else(|e| die(&format!("fake-zfs: lock {}: {e}", p.display())));
    // SAFETY: valid descriptor.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    Locked { _f: f }
}

fn load() -> State {
    match std::fs::read_to_string(state_path()) {
        Ok(t) if !t.trim().is_empty() => serde_json::from_str(&t).unwrap_or_else(|e| die(&format!("fake-zfs: bad state: {e}"))),
        _ => State::default(),
    }
}

fn save(s: &State) {
    let p = state_path();
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s).unwrap()).unwrap();
    std::fs::rename(&tmp, &p).unwrap();
}

fn random_guid() -> u64 {
    let mut b = [0u8; 8];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).unwrap();
    u64::from_le_bytes(b) >> 1
}

fn snap_size() -> u64 {
    std::env::var("FAKEZFS_SNAP_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(65536)
}

fn parent(name: &str) -> Option<&str> {
    name.rsplit_once('/').map(|(p, _)| p)
}

fn not_exist(name: &str) -> ! {
    die(&format!("cannot open '{name}': dataset does not exist"))
}

fn split_snap(full: &str) -> (&str, &str) {
    full.split_once('@').unwrap_or_else(|| die(&format!("'{full}' is not a snapshot")))
}

fn now_hms() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() % 86400;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Parse leading `-x` flags (clusters allowed); flags in `with_arg` take a value.
fn flags(args: &[String], with_arg: &str) -> (Vec<(char, Option<String>)>, Vec<String>) {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') && args[i].len() > 1 {
        let chars: Vec<char> = args[i][1..].chars().collect();
        i += 1;
        for (j, c) in chars.iter().enumerate() {
            if with_arg.contains(*c) {
                let v = if j + 1 < chars.len() {
                    chars[j + 1..].iter().collect()
                } else {
                    i += 1;
                    args.get(i - 1).cloned().unwrap_or_else(|| die(&format!("missing argument for -{c}")))
                };
                out.push((*c, Some(v)));
                break;
            }
            out.push((*c, None));
        }
    }
    (out, args[i..].to_vec())
}

fn has(f: &[(char, Option<String>)], c: char) -> bool {
    f.iter().any(|(x, _)| *x == c)
}

fn val(f: &[(char, Option<String>)], c: char) -> Option<String> {
    f.iter().find(|(x, _)| *x == c).and_then(|(_, v)| v.clone())
}

fn cmd_list(args: &[String]) {
    let (f, rest) = flags(args, "otsdS");
    let cols: Vec<String> =
        val(&f, 'o').unwrap_or_else(|| "name,used,avail,refer,mountpoint".into()).split(',').map(String::from).collect();
    let types: Vec<String> = val(&f, 't')
        .unwrap_or_else(|| "filesystem,volume".into())
        .split(',')
        .map(|t| match t {
            "snap" => "snapshot".to_string(),
            "fs" => "filesystem".to_string(),
            "vol" => "volume".to_string(),
            o => o.to_string(),
        })
        .collect();
    let all = types.iter().any(|t| t == "all");
    let want = |k: &str| all || types.iter().any(|t| t == k);
    let st = load();
    let root = rest.first().cloned();
    if let Some(r) = &root {
        if !st.datasets.contains_key(r) {
            not_exist(r);
        }
    }
    let depth_limit: Option<usize> = val(&f, 'd').map(|d| d.parse().unwrap()).or(if has(&f, 'r') { Some(usize::MAX) } else { None });
    let depth_of = |name: &str| -> Option<usize> {
        match &root {
            None => Some(0),
            Some(r) if name == r => Some(0),
            Some(r) => name.strip_prefix(&format!("{r}/")).map(|rest| rest.split('/').count()),
        }
    };
    let mut rows: Vec<(u64, BTreeMap<&str, String>)> = Vec::new();
    for (name, ds) in &st.datasets {
        let Some(d) = depth_of(name) else { continue };
        let limit = match (&root, depth_limit) {
            (None, _) => usize::MAX,
            (Some(_), Some(l)) => l,
            (Some(_), None) => 0,
        };
        if d <= limit && want(&ds.kind) {
            let mut m = BTreeMap::new();
            m.insert("name", name.clone());
            m.insert("type", ds.kind.clone());
            m.insert("guid", ds.guid.to_string());
            m.insert("createtxg", ds.createtxg.to_string());
            m.insert("used", (ds.snaps.len() as u64 * 1000 + 1000).to_string());
            m.insert("avail", "1000000000".into());
            m.insert("refer", "1000".into());
            m.insert("mountpoint", if ds.kind == "filesystem" { format!("/{name}") } else { "-".into() });
            rows.push((ds.createtxg, m));
        }
        // Snapshots and bookmarks are one level below their dataset (root's own at depth 1).
        let child_ok = root.is_none() || d < limit_plus(depth_limit, &root);
        if child_ok {
            for (kind, sep, list) in [("snapshot", '@', &ds.snaps), ("bookmark", '#', &ds.bookmarks)] {
                if !want(kind) {
                    continue;
                }
                for s in list {
                    let mut m = BTreeMap::new();
                    m.insert("name", format!("{name}{sep}{}", s.name));
                    m.insert("type", kind.to_string());
                    m.insert("guid", s.guid.to_string());
                    m.insert("createtxg", s.createtxg.to_string());
                    m.insert("used", if kind == "snapshot" { "0".into() } else { "-".into() });
                    m.insert("avail", "-".into());
                    m.insert("refer", if kind == "snapshot" { s.size.to_string() } else { "-".into() });
                    m.insert("mountpoint", "-".into());
                    rows.push((s.createtxg, m));
                }
            }
        }
    }
    if val(&f, 's').as_deref() == Some("createtxg") {
        rows.sort_by_key(|(t, _)| *t);
    }
    let mut out = String::new();
    for (_, m) in rows {
        let line: Vec<String> = cols.iter().map(|c| m.get(c.as_str()).cloned().unwrap_or_else(|| "-".into())).collect();
        out.push_str(&line.join("\t"));
        out.push('\n');
    }
    print!("{out}");
}

/// Snapshots of a dataset at depth d are at d+1: allowed when d+1 <= limit.
fn limit_plus(limit: Option<usize>, root: &Option<String>) -> usize {
    match (root, limit) {
        (None, _) => usize::MAX,
        (Some(_), Some(l)) => l,
        // `zfs list -t snapshot pool/a` (no -r/-d) lists pool/a's snapshots.
        (Some(_), None) => 1,
    }
}

fn cmd_create(args: &[String]) {
    let (f, rest) = flags(args, "Vo");
    let name = rest.first().unwrap_or_else(|| die("missing dataset"));
    let _l = lock();
    let mut st = load();
    if st.datasets.contains_key(name) {
        die(&format!("cannot create '{name}': dataset already exists"));
    }
    let mut missing = Vec::new();
    let mut p = parent(name);
    while let Some(pp) = p {
        if st.datasets.contains_key(pp) {
            break;
        }
        missing.push(pp.to_string());
        p = parent(pp);
    }
    if parent(name).is_none() {
        // a pool
    } else if !missing.is_empty() && !has(&f, 'p') {
        die(&format!("cannot create '{name}': parent does not exist"));
    }
    for m in missing.into_iter().rev() {
        st.txg += 1;
        st.datasets.insert(m, Ds { kind: "filesystem".into(), guid: random_guid(), createtxg: st.txg, ..Default::default() });
    }
    st.txg += 1;
    let kind = if has(&f, 'V') { "volume" } else { "filesystem" };
    st.datasets.insert(name.clone(), Ds { kind: kind.into(), guid: random_guid(), createtxg: st.txg, ..Default::default() });
    save(&st);
}

fn cmd_snapshot(args: &[String]) {
    let (f, rest) = flags(args, "o");
    let _l = lock();
    let mut st = load();
    let mut plan = Vec::new();
    for full in &rest {
        let (ds, snap) = split_snap(full);
        let Some(d) = st.datasets.get(ds) else { die(&format!("cannot create snapshot '{full}': dataset does not exist")) };
        if d.snaps.iter().any(|s| s.name == snap) {
            die(&format!("cannot create snapshot '{full}': dataset already exists"));
        }
        plan.push((ds.to_string(), snap.to_string()));
        if has(&f, 'r') {
            for child in st.datasets.keys().filter(|k| k.starts_with(&format!("{ds}/"))) {
                plan.push((child.clone(), snap.to_string()));
            }
        }
    }
    // One txg for the whole command, like an atomic `zfs snapshot a@x b@x`.
    st.txg += 1;
    let txg = st.txg;
    for (ds, snap) in plan {
        let d = st.datasets.get_mut(&ds).unwrap();
        d.snaps.push(SnapRec { name: snap, guid: random_guid(), createtxg: txg, size: snap_size() });
        d.dirty = false;
    }
    save(&st);
}

fn cmd_destroy(args: &[String]) {
    let (f, rest) = flags(args, "");
    let name = rest.first().unwrap_or_else(|| die("missing name"));
    let _l = lock();
    let mut st = load();
    if let Some((ds, snap)) = name.split_once('@') {
        let d = st.datasets.get_mut(ds).unwrap_or_else(|| not_exist(ds));
        let before = d.snaps.len();
        d.snaps.retain(|s| s.name != snap);
        if d.snaps.len() == before {
            die("could not find any snapshots to destroy; check snapshot names.");
        }
    } else if let Some((ds, bm)) = name.split_once('#') {
        let d = st.datasets.get_mut(ds).unwrap_or_else(|| not_exist(ds));
        d.bookmarks.retain(|s| s.name != bm);
    } else {
        let d = st.datasets.get(name).unwrap_or_else(|| not_exist(name));
        let children: Vec<String> = st.datasets.keys().filter(|k| k.starts_with(&format!("{name}/"))).cloned().collect();
        if !has(&f, 'r') && (!children.is_empty() || !d.snaps.is_empty()) {
            die(&format!("cannot destroy '{name}': filesystem has children\nuse '-r' to destroy the following datasets:"));
        }
        for c in children {
            st.datasets.remove(&c);
        }
        st.datasets.remove(name);
    }
    save(&st);
}

fn cmd_rename(args: &[String]) {
    let (f, rest) = flags(args, "");
    let (from, to) = match rest.as_slice() {
        [a, b] => (a.clone(), b.clone()),
        _ => die("usage: rename from to"),
    };
    let _l = lock();
    let mut st = load();
    if let Some((ds, snap)) = from.split_once('@') {
        let new = to.split_once('@').map(|(_, s)| s.to_string()).unwrap_or(to.clone());
        let d = st.datasets.get_mut(ds).unwrap_or_else(|| not_exist(ds));
        let s = d.snaps.iter_mut().find(|s| s.name == snap).unwrap_or_else(|| not_exist(&from));
        s.name = new;
        save(&st);
        return;
    }
    if !st.datasets.contains_key(&from) {
        not_exist(&from);
    }
    if st.datasets.contains_key(&to) {
        die(&format!("cannot rename to '{to}': dataset already exists"));
    }
    if let Some(p) = parent(&to) {
        if !st.datasets.contains_key(p) && !has(&f, 'p') {
            die(&format!("cannot rename to '{to}': parent does not exist"));
        }
    }
    let moving: Vec<String> = st.datasets.keys().filter(|k| *k == &from || k.starts_with(&format!("{from}/"))).cloned().collect();
    for k in moving {
        let d = st.datasets.remove(&k).unwrap();
        st.datasets.insert(format!("{to}{}", &k[from.len()..]), d);
    }
    save(&st);
}

fn cmd_bookmark(args: &[String]) {
    let [snap, bm] = args else { die("usage: bookmark snap bookmark") };
    let _l = lock();
    let mut st = load();
    let (ds, sname) = split_snap(snap);
    let bname = bm.split_once('#').map(|(_, b)| b).unwrap_or(bm);
    let d = st.datasets.get_mut(ds).unwrap_or_else(|| not_exist(ds));
    let s = d.snaps.iter().find(|s| s.name == sname).cloned().unwrap_or_else(|| not_exist(snap));
    if d.bookmarks.iter().any(|b| b.name == bname) {
        die(&format!("cannot create bookmark '{bm}': bookmark exists"));
    }
    d.bookmarks.push(SnapRec { name: bname.into(), ..s });
    save(&st);
}

fn cmd_get(args: &[String]) {
    let (_f, rest) = flags(args, "os");
    let [prop, ds] = rest.as_slice() else { die("usage: get prop ds") };
    let st = load();
    let d = st.datasets.get(ds).unwrap_or_else(|| not_exist(ds));
    let v = match prop.as_str() {
        "receive_resume_token" => d.resume_token.clone().unwrap_or_else(|| "-".into()),
        "guid" => d.guid.to_string(),
        "type" => d.kind.clone(),
        _ => "-".into(),
    };
    println!("{v}");
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn token_for(h: &Header, offset: u64) -> String {
    let mut h = h.clone();
    h.resume_offset = offset;
    format!("1-{}", hex(serde_json::to_string(&h).unwrap().as_bytes()))
}

fn header_from_token(token: &str) -> Header {
    let body = token.strip_prefix("1-").unwrap_or_else(|| die("cannot resume send: invalid token"));
    serde_json::from_slice(&unhex(body)).unwrap_or_else(|_| die("cannot resume send: invalid token"))
}

fn cmd_send(args: &[String]) {
    let (f, rest) = flags(args, "iIt");
    let verbose = has(&f, 'v');
    let dry = has(&f, 'n');
    let st = load();
    let header = if let Some(token) = val(&f, 't') {
        let h = header_from_token(&token);
        let src = st.datasets.get(&h.source).unwrap_or_else(|| die("cannot resume send: source dataset no longer exists"));
        let last = h.snaps.last().unwrap();
        if !src.snaps.iter().any(|s| s.guid == last.guid) {
            die("cannot resume send: snapshot no longer exists");
        }
        h
    } else {
        let full = rest.first().unwrap_or_else(|| die("missing snapshot argument"));
        let (ds, snap) = split_snap(full);
        let d = st.datasets.get(ds).unwrap_or_else(|| not_exist(full));
        let to_idx = d.snaps.iter().position(|s| s.name == snap).unwrap_or_else(|| not_exist(full));
        let from = val(&f, 'I').map(|v| (v, true)).or_else(|| val(&f, 'i').map(|v| (v, false)));
        let replication = has(&f, 'R');
        let (from_guid, snaps, from_name) = match &from {
            None if replication => (None, d.snaps[..=to_idx].to_vec(), None),
            None => (None, vec![d.snaps[to_idx].clone()], None),
            Some((base, intermediates)) => {
                let (bds, bname, is_bm) = if let Some((a, b)) = base.split_once('#') {
                    (a, b, true)
                } else if let Some((a, b)) = base.split_once('@') {
                    (if a.is_empty() { ds } else { a }, b, false)
                } else {
                    die(&format!("invalid incremental source '{base}'"))
                };
                if bds != ds {
                    die("incremental source must be in same filesystem");
                }
                let rec = if is_bm { d.bookmarks.iter().find(|b| b.name == bname) } else { d.snaps.iter().find(|s| s.name == bname) }
                    .cloned()
                    .unwrap_or_else(|| not_exist(base));
                if rec.createtxg >= d.snaps[to_idx].createtxg {
                    die("incremental source must be earlier than the target");
                }
                let snaps = if *intermediates && !is_bm {
                    d.snaps.iter().filter(|s| s.createtxg > rec.createtxg && s.createtxg <= d.snaps[to_idx].createtxg).cloned().collect()
                } else {
                    vec![d.snaps[to_idx].clone()]
                };
                (Some(rec.guid), snaps, Some(bname.to_string()))
            }
        };
        let mut children = Vec::new();
        if replication {
            for (cname, c) in st.datasets.iter().filter(|(k, _)| k.starts_with(&format!("{ds}/"))) {
                let Some(cto) = c.snaps.iter().find(|s| s.name == snap) else { continue };
                let after = from_name.as_ref().and_then(|n| c.snaps.iter().find(|s| &s.name == n)).map(|s| s.createtxg).unwrap_or(0);
                let cs: Vec<SnapRec> = c.snaps.iter().filter(|s| s.createtxg > after && s.createtxg <= cto.createtxg).cloned().collect();
                children.push(ChildStream { relname: cname[ds.len() + 1..].to_string(), kind: c.kind.clone(), snaps: cs });
            }
        }
        Header {
            magic: MAGIC.into(),
            full: from.is_none(),
            replication,
            source: ds.to_string(),
            kind: d.kind.clone(),
            from_guid,
            snaps,
            children,
            resume_offset: 0,
        }
    };
    let total = header.total();
    let remaining = total.saturating_sub(header.resume_offset);
    let to_name = format!("{}@{}", header.source, header.snaps.last().map(|s| s.name.as_str()).unwrap_or(""));
    if dry || verbose {
        let line = if header.full { format!("full\t{to_name}\t{remaining}") } else { format!("incremental\t-\t{to_name}\t{remaining}") };
        let text = format!("{line}\nsize\t{remaining}\n");
        if dry {
            print!("{text}");
            return;
        }
        eprint!("{text}");
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let fail_after: Option<u64> = std::env::var("FAKEZFS_FAIL_ONCE_FILE").ok().filter(|p| std::path::Path::new(p).exists()).map(|p| {
        let _ = std::fs::remove_file(p);
        std::env::var("FAKEZFS_FAIL_AFTER_BYTES").ok().and_then(|v| v.parse().ok()).unwrap_or(remaining / 2)
    });
    let write = |out: &mut dyn Write, b: &[u8]| {
        if out.write_all(b).is_err() {
            die("send: broken pipe");
        }
    };
    write(&mut out, format!("{}\n", serde_json::to_string(&header).unwrap()).as_bytes());
    let chunk = vec![0xA5u8; 8192];
    let mut sent = 0u64;
    while sent < remaining {
        let n = (remaining - sent).min(chunk.len() as u64);
        if let Some(limit) = fail_after {
            if sent + n > limit {
                write(&mut out, &chunk[..(limit - sent) as usize]);
                let _ = out.flush();
                die("send: simulated failure: connection lost");
            }
        }
        write(&mut out, &chunk[..n as usize]);
        sent += n;
        if verbose && (sent % (32 * 1024) == 0 || sent == remaining) {
            eprintln!("{}\t{}\t{to_name}", now_hms(), sent + header.resume_offset);
        }
    }
    write(&mut out, TRAILER);
    let _ = out.flush();
}

fn add_snaps(st: &mut State, target: &str, kind: &str, snaps: &[SnapRec]) {
    let exists = st.datasets.contains_key(target);
    if !exists {
        st.txg += 1;
        let txg = st.txg;
        st.datasets.insert(target.into(), Ds { kind: kind.into(), guid: random_guid(), createtxg: txg, ..Default::default() });
    }
    for s in snaps {
        st.txg += 1;
        let txg = st.txg;
        let d = st.datasets.get_mut(target).unwrap();
        if !d.snaps.iter().any(|x| x.guid == s.guid) {
            d.snaps.push(SnapRec { createtxg: txg, ..s.clone() });
        }
    }
    let d = st.datasets.get_mut(target).unwrap();
    d.partial = false;
    d.resume_token = None;
    d.dirty = false;
}

/// Check the stream can be received into `target`; with `-F`, perform the rollback/overwrite.
fn validate(st: &mut State, h: &Header, target: &str, force: bool) {
    if h.full {
        if let Some(d) = st.datasets.get(target) {
            if d.partial || d.resume_token.is_some() {
                die(&format!(
                    "cannot receive new filesystem stream: destination {target} contains partially-complete state from \"zfs receive -s\"."
                ));
            }
            if !force {
                die(&format!("cannot receive new filesystem stream: destination '{target}' exists\nmust specify -F to overwrite it"));
            }
            if !d.snaps.is_empty() && !h.replication {
                die(&format!(
                    "cannot receive new filesystem stream: destination has snapshots (eg. {target}@{})\nmust destroy them to overwrite it",
                    d.snaps[0].name
                ));
            }
            // -F with a replication stream: replace the destination.
            let kids: Vec<String> = st.datasets.keys().filter(|k| k.starts_with(&format!("{target}/"))).cloned().collect();
            for k in kids {
                st.datasets.remove(&k);
            }
            st.datasets.remove(target);
        } else if let Some(p) = parent(target) {
            if !st.datasets.contains_key(p) {
                die(&format!("cannot receive new filesystem stream: parent of '{target}' does not exist"));
            }
        }
        return;
    }
    let d = st
        .datasets
        .get_mut(target)
        .unwrap_or_else(|| die(&format!("cannot receive incremental stream: destination '{target}' does not exist")));
    if d.resume_token.is_some() {
        die(&format!("cannot receive incremental stream: destination {target} contains partially-complete state from \"zfs receive -s\"."));
    }
    let from = h.from_guid.unwrap();
    let Some(pos) = d.snaps.iter().position(|s| s.guid == from) else {
        die(&format!("cannot receive incremental stream: most recent snapshot of {target} does not\nmatch incremental source"));
    };
    if pos + 1 != d.snaps.len() || d.dirty {
        if !force {
            die(&format!("cannot receive incremental stream: destination {target} has been modified\nsince most recent snapshot"));
        }
        d.snaps.truncate(pos + 1);
        d.dirty = false;
    }
    if let Some(s) = h.snaps.iter().find(|s| d.snaps.iter().any(|x| x.guid == s.guid)) {
        die(&format!("cannot receive incremental stream: destination already exists ({target}@{})", s.name));
    }
}

fn cmd_receive(args: &[String]) {
    let (f, rest) = flags(args, "o");
    let target = rest.first().cloned().unwrap_or_else(|| die("missing dataset"));
    if has(&f, 'A') {
        let _l = lock();
        let mut st = load();
        let d = st.datasets.get_mut(&target).unwrap_or_else(|| not_exist(&target));
        if d.resume_token.is_none() {
            die(&format!("'{target}' does not have any resumable receive state to abort"));
        }
        if d.partial {
            st.datasets.remove(&target);
        } else {
            d.resume_token = None;
        }
        save(&st);
        return;
    }
    let force = has(&f, 'F');
    let resumable = has(&f, 's');
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut line = Vec::new();
    if input.read_until(b'\n', &mut line).unwrap_or(0) == 0 {
        die("cannot receive: failed to read from stream");
    }
    let header: Header = serde_json::from_slice(&line).unwrap_or_else(|_| die("cannot receive: invalid stream (bad magic number)"));
    if header.magic != MAGIC {
        die("cannot receive: invalid stream (bad magic number)");
    }
    if has(&f, 'v') {
        println!(
            "receiving {} stream of {}@{} into {target}",
            if header.full { "full" } else { "incremental" },
            header.source,
            header.snaps.last().map(|s| s.name.as_str()).unwrap_or("")
        );
    }
    {
        let _l = lock();
        let mut st = load();
        if header.resume_offset > 0 {
            let d = st.datasets.get(&target).unwrap_or_else(|| not_exist(&target));
            let stored = d.resume_token.as_deref().map(header_from_token);
            match stored {
                Some(s)
                    if s.resume_offset == header.resume_offset && s.snaps.last().map(|x| x.guid) == header.snaps.last().map(|x| x.guid) => {
                }
                _ => die("cannot receive resume stream: kernel modules must be upgraded to receive this stream (resume token mismatch)"),
            }
        } else {
            validate(&mut st, &header, &target, force);
            save(&st);
        }
    }
    let total = header.total();
    let need = total - header.resume_offset;
    let mut got = 0u64;
    let mut buf = vec![0u8; 8192];
    while got < need {
        let want = (need - got).min(buf.len() as u64) as usize;
        match input.read(&mut buf[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n as u64,
        }
    }
    let mut trailer = vec![0u8; TRAILER.len()];
    let complete = got == need && input.read_exact(&mut trailer).is_ok() && trailer == TRAILER;
    let _l = lock();
    let mut st = load();
    if !complete {
        if resumable {
            let offset = header.resume_offset + got;
            let tok = token_for(&Header { resume_offset: 0, ..header.clone() }, offset);
            if !st.datasets.contains_key(&target) {
                st.txg += 1;
                let txg = st.txg;
                st.datasets.insert(
                    target.clone(),
                    Ds { kind: header.kind.clone(), guid: random_guid(), createtxg: txg, partial: true, ..Default::default() },
                );
            }
            st.datasets.get_mut(&target).unwrap().resume_token = Some(tok);
            save(&st);
            die(&format!("cannot receive {} stream: checksum mismatch or incomplete stream.\nPartially received snapshot is saved.\nA resuming stream can be generated on the sending system by running:\n    zfs send -t <token>", if header.full { "new filesystem" } else { "incremental" }));
        }
        save(&st);
        die("cannot receive: failed to read from stream");
    }
    add_snaps(&mut st, &target, &header.kind, &header.snaps);
    for c in &header.children {
        add_snaps(&mut st, &format!("{target}/{}", c.relname), &c.kind, &c.snaps);
    }
    save(&st);
}

fn cmd_fake_write(args: &[String]) {
    let name = args.first().unwrap_or_else(|| die("missing dataset"));
    let _l = lock();
    let mut st = load();
    st.datasets.get_mut(name).unwrap_or_else(|| not_exist(name)).dirty = true;
    save(&st);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(log) = std::env::var("FAKEZFS_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            let _ = writeln!(f, "{} zfs {}", std::env::var("FAKE_SSH_HOST").unwrap_or_else(|_| "local".into()), args.join(" "));
        }
    }
    let Some((cmd, rest)) = args.split_first() else { die("usage: zfs command args ...") };
    match cmd.as_str() {
        "list" => cmd_list(rest),
        "create" => cmd_create(rest),
        "snapshot" | "snap" => cmd_snapshot(rest),
        "destroy" => cmd_destroy(rest),
        "rename" => cmd_rename(rest),
        "bookmark" => cmd_bookmark(rest),
        "get" => cmd_get(rest),
        "send" => cmd_send(rest),
        "receive" | "recv" => cmd_receive(rest),
        "fake-write" => cmd_fake_write(rest),
        "fake-dump" => println!("{}", serde_json::to_string_pretty(&load()).unwrap()),
        other => die(&format!("unrecognized command '{other}'")),
    }
}
