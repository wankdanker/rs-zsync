//! The `zfs` command line: argv builders and output parsers. Nothing here runs anything.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Columns zsync asks `zfs list` for (`-H -p`, tab separated).
pub const LIST_COLUMNS: &str = "name,type,guid,createtxg,used,avail,refer,mountpoint";

/// One row of `zfs list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dataset {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub createtxg: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avail: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refer: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mountpoint: Option<String>,
}

impl Dataset {
    /// `pool/a` for `pool/a@snap` / `pool/a#bm`, else the name itself.
    pub fn dataset(&self) -> &str {
        crate::naming::split_snapshot(&self.name).map(|(d, _)| d).unwrap_or(&self.name)
    }
    /// The part after `@`/`#`.
    pub fn short(&self) -> &str {
        crate::naming::split_snapshot(&self.name).map(|(_, s)| s).unwrap_or(&self.name)
    }
    pub fn is_snapshot(&self) -> bool {
        self.kind == "snapshot"
    }
    pub fn is_bookmark(&self) -> bool {
        self.kind == "bookmark"
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListQuery {
    /// Dataset to start from; `None` lists everything.
    #[serde(default)]
    pub root: Option<String>,
    /// `-t`, e.g. `filesystem,volume`, `snapshot`, `snap`.
    #[serde(default)]
    pub types: Option<String>,
    #[serde(default)]
    pub recursive: bool,
    /// `-d N` (implies recursion to that depth).
    #[serde(default)]
    pub depth: Option<u32>,
    /// `-s createtxg`: oldest first, the order snapshot logic relies on.
    #[serde(default)]
    pub sort_createtxg: bool,
}

impl ListQuery {
    /// Snapshots (and bookmarks) of exactly `dataset`, oldest first.
    pub fn snapshots_of(dataset: &str, bookmarks: bool) -> ListQuery {
        ListQuery {
            root: Some(dataset.to_string()),
            types: Some(if bookmarks { "snapshot,bookmark" } else { "snapshot" }.into()),
            recursive: false,
            depth: Some(1),
            sort_createtxg: true,
        }
    }
}

pub fn list_argv(zfs: &[String], q: &ListQuery) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.extend(["list", "-H", "-p", "-o", LIST_COLUMNS].map(String::from));
    if let Some(t) = q.types.as_ref().filter(|t| !t.is_empty()) {
        argv.push("-t".into());
        argv.push(t.clone());
    }
    if q.sort_createtxg {
        argv.push("-s".into());
        argv.push("createtxg".into());
    }
    if let Some(d) = q.depth {
        argv.push("-d".into());
        argv.push(d.to_string());
    } else if q.recursive {
        argv.push("-r".into());
    }
    if let Some(root) = q.root.as_ref().filter(|r| !r.is_empty()) {
        argv.push(root.clone());
    }
    argv
}

fn num(field: &str) -> Option<u64> {
    if field == "-" || field.is_empty() {
        None
    } else {
        field.parse().ok()
    }
}

/// Parse `zfs list -H -p -o LIST_COLUMNS` output.
pub fn parse_list(stdout: &str) -> Result<Vec<Dataset>> {
    let mut out = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 8 {
            bail!("unexpected zfs list output: {line}");
        }
        out.push(Dataset {
            name: f[0].to_string(),
            kind: f[1].to_string(),
            guid: num(f[2]),
            createtxg: num(f[3]),
            used: num(f[4]),
            avail: num(f[5]),
            refer: num(f[6]),
            mountpoint: Some(f[7]).filter(|m| *m != "-" && !m.is_empty()).map(String::from),
        });
    }
    Ok(out)
}

/// What to send.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendSpec {
    /// `pool/a@snap` (ignored when `resume_token` is set).
    pub snapshot: String,
    /// Incremental base: `pool/a@snap` or `pool/a#bookmark`.
    #[serde(default)]
    pub from: Option<String>,
    /// `-I` (all intermediate snapshots) instead of `-i`.
    #[serde(default)]
    pub intermediary: bool,
    #[serde(default)]
    pub replication: bool,
    #[serde(default)]
    pub properties: bool,
    #[serde(default)]
    pub compressed: bool,
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub large_block: bool,
    /// `-v -P`: parsable progress on stderr.
    #[serde(default)]
    pub progress: bool,
    /// `-n -v -P`: estimate only.
    #[serde(default)]
    pub dry_run: bool,
    /// `-t TOKEN`: resume an interrupted receive.
    #[serde(default)]
    pub resume_token: Option<String>,
}

pub fn send_argv(zfs: &[String], s: &SendSpec) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.push("send".into());
    if let Some(token) = &s.resume_token {
        if s.dry_run {
            argv.push("-n".into());
        }
        if s.progress || s.dry_run {
            argv.extend(["-v".into(), "-P".into()]);
        }
        argv.push("-t".into());
        argv.push(token.clone());
        return argv;
    }
    let flags: [(bool, &str); 8] = [
        (s.replication, "-R"),
        (s.properties, "-p"),
        (s.dry_run, "-n"),
        (s.progress || s.dry_run, "-P"),
        (s.progress || s.dry_run, "-v"),
        (s.compressed, "-c"),
        (s.raw, "-w"),
        (s.large_block, "-L"),
    ];
    argv.extend(flags.iter().filter(|(on, _)| *on).map(|(_, f)| f.to_string()));
    if let Some(from) = s.from.as_ref().filter(|f| !f.is_empty()) {
        // -I needs a snapshot; a bookmark base only supports -i.
        argv.push(if s.intermediary && !from.contains('#') { "-I" } else { "-i" }.into());
        argv.push(from.clone());
    }
    argv.push(s.snapshot.clone());
    argv
}

/// How to receive.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecvSpec {
    pub dataset: String,
    #[serde(default)]
    pub force: bool,
    /// `-s`: keep partial state so the transfer can resume.
    #[serde(default)]
    pub resumable: bool,
    /// `-u`: don't mount.
    #[serde(default)]
    pub unmounted: bool,
    #[serde(default)]
    pub verbose: bool,
}

pub fn recv_argv(zfs: &[String], r: &RecvSpec) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.push("receive".into());
    let flags = [(r.verbose, "-v"), (r.force, "-F"), (r.resumable, "-s"), (r.unmounted, "-u")];
    argv.extend(flags.iter().filter(|(on, _)| *on).map(|(_, f)| f.to_string()));
    argv.push(r.dataset.clone());
    argv
}

pub fn recv_abort_argv(zfs: &[String], dataset: &str) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.extend(["receive".into(), "-A".into(), dataset.into()]);
    argv
}

pub fn snapshot_argv(zfs: &[String], names: &[String], recursive: bool) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.push("snapshot".into());
    if recursive {
        argv.push("-r".into());
    }
    argv.extend(names.iter().cloned());
    argv
}

pub fn destroy_argv(zfs: &[String], name: &str, recursive: bool) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.push("destroy".into());
    if recursive {
        argv.push("-r".into());
    }
    argv.push(name.into());
    argv
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenameOpts {
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub parents: bool,
}

pub fn rename_argv(zfs: &[String], from: &str, to: &str, o: &RenameOpts) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.push("rename".into());
    let flags = [(o.recursive, "-r"), (o.force, "-f"), (o.parents, "-p")];
    argv.extend(flags.iter().filter(|(on, _)| *on).map(|(_, f)| f.to_string()));
    argv.push(from.into());
    argv.push(to.into());
    argv
}

pub fn bookmark_argv(zfs: &[String], snapshot: &str, bookmark: &str) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.extend(["bookmark".into(), snapshot.into(), bookmark.into()]);
    argv
}

pub fn get_argv(zfs: &[String], property: &str, dataset: &str) -> Vec<String> {
    let mut argv = zfs.to_vec();
    argv.extend(["get", "-H", "-p", "-o", "value", property, dataset].map(String::from));
    argv
}

/// `-` means unset for `zfs get`.
pub fn parse_get(stdout: &str) -> Option<String> {
    let v = stdout.lines().next().unwrap_or("").trim();
    (!v.is_empty() && v != "-").then(|| v.to_string())
}

/// The total from `zfs send -nvP` (`size\t12345`). OpenZFS prints it on stdout or stderr
/// depending on version, so callers pass both.
pub fn parse_estimate(text: &str) -> Option<u64> {
    text.lines().rev().find_map(|l| {
        let mut f = l.split_whitespace();
        (f.next() == Some("size")).then(|| f.next()?.parse().ok()).flatten()
    })
}

/// A `zfs send -v` progress line: `HH:MM:SS  bytes  pool/a@snap` (bytes raw with -P, else
/// human-readable like `1.21M`).
pub fn parse_progress_line(line: &str) -> Option<(u64, String)> {
    let mut f = line.split_whitespace();
    let time = f.next()?;
    let b = time.as_bytes();
    if !(b.len() == 8 && b[2] == b':' && b[5] == b':' && time.chars().filter(|c| c.is_ascii_digit()).count() == 6) {
        return None;
    }
    let bytes = f.next()?;
    let snap = f.next()?.to_string();
    let bytes = bytes.parse::<u64>().ok().or_else(|| crate::util::parse_size(bytes).ok())?;
    Some((bytes, snap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z() -> Vec<String> {
        vec!["zfs".into()]
    }

    #[test]
    fn list_args() {
        let q = ListQuery { root: Some("pool".into()), types: Some("filesystem".into()), recursive: true, ..Default::default() };
        assert_eq!(list_argv(&z(), &q).join(" "), format!("zfs list -H -p -o {LIST_COLUMNS} -t filesystem -r pool"));
        let q = ListQuery::snapshots_of("pool/a", true);
        assert_eq!(
            list_argv(&z(), &q).join(" "),
            format!("zfs list -H -p -o {LIST_COLUMNS} -t snapshot,bookmark -s createtxg -d 1 pool/a")
        );
        let sudo: Vec<String> = vec!["sudo".into(), "-n".into(), "zfs".into()];
        assert_eq!(list_argv(&sudo, &ListQuery::default())[..4], ["sudo", "-n", "zfs", "list"]);
    }

    #[test]
    fn parse_rows() {
        let text = "pool\tfilesystem\t123\t1\t1000\t5000\t96\t/pool\n\
                    pool@s1\tsnapshot\t456\t7\t0\t-\t96\t-\n\
                    pool#b\tbookmark\t456\t7\t-\t-\t-\t-\n";
        let rows = parse_list(text).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].mountpoint.as_deref(), Some("/pool"));
        assert_eq!(rows[1].avail, None);
        assert_eq!(rows[1].dataset(), "pool");
        assert_eq!(rows[1].short(), "s1");
        assert!(rows[2].is_bookmark());
        assert_eq!(rows[2].used, None);
        assert!(parse_list("garbage\n").is_err());
        assert!(parse_list("").unwrap().is_empty());
    }

    #[test]
    fn send_args() {
        let mut s = SendSpec { snapshot: "p/a@2".into(), ..Default::default() };
        assert_eq!(send_argv(&z(), &s).join(" "), "zfs send p/a@2");
        s.from = Some("p/a@1".into());
        s.intermediary = true;
        s.replication = true;
        s.compressed = true;
        s.raw = true;
        assert_eq!(send_argv(&z(), &s).join(" "), "zfs send -R -c -w -I p/a@1 p/a@2");
        s.intermediary = false;
        s.replication = false;
        s.compressed = false;
        s.raw = false;
        s.properties = true;
        s.progress = true;
        assert_eq!(send_argv(&z(), &s).join(" "), "zfs send -p -P -v -i p/a@1 p/a@2");
        s.from = Some("p/a#1".into());
        s.intermediary = true;
        s.progress = false;
        s.properties = false;
        assert_eq!(send_argv(&z(), &s).join(" "), "zfs send -i p/a#1 p/a@2");
        s.dry_run = true;
        assert_eq!(send_argv(&z(), &s).join(" "), "zfs send -n -P -v -i p/a#1 p/a@2");
        let r = SendSpec { resume_token: Some("1-abc".into()), progress: true, ..Default::default() };
        assert_eq!(send_argv(&z(), &r).join(" "), "zfs send -v -P -t 1-abc");
    }

    #[test]
    fn recv_and_admin_args() {
        let r = RecvSpec { dataset: "b/a".into(), force: true, resumable: true, unmounted: true, verbose: false };
        assert_eq!(recv_argv(&z(), &r).join(" "), "zfs receive -F -s -u b/a");
        assert_eq!(recv_abort_argv(&z(), "b/a").join(" "), "zfs receive -A b/a");
        assert_eq!(snapshot_argv(&z(), &["a@1".into(), "b@1".into()], false).join(" "), "zfs snapshot a@1 b@1");
        assert_eq!(destroy_argv(&z(), "a", true).join(" "), "zfs destroy -r a");
        let o = RenameOpts { recursive: false, force: true, parents: true };
        assert_eq!(rename_argv(&z(), "a", "b", &o).join(" "), "zfs rename -f -p a b");
        assert_eq!(bookmark_argv(&z(), "a@1", "a#1").join(" "), "zfs bookmark a@1 a#1");
        assert_eq!(get_argv(&z(), "receive_resume_token", "a").join(" "), "zfs get -H -p -o value receive_resume_token a");
        assert_eq!(parse_get("-\n"), None);
        assert_eq!(parse_get("1-abc\n"), Some("1-abc".into()));
    }

    #[test]
    fn estimates_and_progress() {
        assert_eq!(parse_estimate("incremental\tp/a@1\tp/a@2\t1000\nsize\t1000\n"), Some(1000));
        assert_eq!(parse_estimate("nothing"), None);
        assert_eq!(parse_progress_line("12:00:01\t1048576\tp/a@2"), Some((1048576, "p/a@2".into())));
        assert_eq!(parse_progress_line("12:00:01   1.00M   p/a@2"), Some((1048576, "p/a@2".into())));
        assert_eq!(parse_progress_line("full\tp/a@2\t100"), None);
        assert_eq!(parse_progress_line("send from @1 to p/a@2 estimated size is 1M"), None);
    }
}
