//! Helpers for Tab completion (clyops `complete`): working out which slot of a partly typed
//! command line is being completed, and finding host names and snapshot tags to offer.
//!
//! The completers themselves live in `main.rs`, next to the option definitions; everything here
//! is free of I/O except reading the ssh files.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// The options of one command: short letters and which longs take a value.
#[derive(Clone, Debug, Default)]
pub struct OptionShapes {
    pub shorts: HashMap<char, String>,
    pub takes_value: HashSet<String>,
}

impl OptionShapes {
    pub fn add(&mut self, long: &str, short: &str, takes_value: bool) {
        if let Some(c) = short.chars().next() {
            self.shorts.insert(c, long.to_string());
        }
        if takes_value {
            self.takes_value.insert(long.to_string());
        }
    }
}

/// What the typed words amount to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Walk {
    /// Positional words, in order.
    pub positionals: Vec<String>,
    /// The long name of an option at the end that still needs its value (being completed now).
    pub pending: Option<String>,
    /// The words with a trailing value-less option removed, safe to parse.
    pub parseable: Vec<String>,
}

/// Walk the words typed so far (not including the one being completed).
pub fn walk(words: &[String], shapes: &OptionShapes) -> Walk {
    let mut w = Walk::default();
    let mut end_of_options = false;
    for word in words {
        if w.pending.take().is_some() {
            w.parseable.push(word.clone());
            continue;
        }
        w.parseable.push(word.clone());
        if end_of_options || word == "-" || !word.starts_with('-') {
            w.positionals.push(word.clone());
        } else if word == "--" {
            end_of_options = true;
        } else if let Some(body) = word.strip_prefix("--") {
            if !body.contains('=') && shapes.takes_value.contains(body) {
                w.pending = Some(body.to_string());
            }
        } else {
            let chars: Vec<char> = word[1..].chars().collect();
            for (i, c) in chars.iter().enumerate() {
                match shapes.shorts.get(c) {
                    Some(long) if shapes.takes_value.contains(long) => {
                        if i + 1 == chars.len() {
                            w.pending = Some(long.clone());
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    if w.pending.is_some() {
        w.parseable.pop();
    }
    w
}

/// Host names from `~/.ssh/config` (`Host` entries without patterns) and `~/.ssh/known_hosts`
/// (plain entries; hashed ones can't be read back), sorted and without duplicates.
pub fn ssh_hosts(home: &Path) -> Vec<String> {
    let mut hosts = BTreeSet::new();
    if let Ok(text) = std::fs::read_to_string(home.join(".ssh/config")) {
        hosts.extend(config_hosts(&text));
    }
    if let Ok(text) = std::fs::read_to_string(home.join(".ssh/known_hosts")) {
        hosts.extend(known_hosts(&text));
    }
    hosts.into_iter().collect()
}

pub fn config_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let mut parts = line.splitn(2, |c: char| c.is_whitespace() || c == '=');
        let (Some(key), Some(rest)) = (parts.next(), parts.next()) else { continue };
        if !key.eq_ignore_ascii_case("host") {
            continue;
        }
        for name in rest.split_whitespace() {
            let name = name.trim_matches('"');
            if !name.is_empty() && !name.contains(['*', '?', '!']) {
                out.push(name.to_string());
            }
        }
    }
    out
}

pub fn known_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('|') {
            continue;
        }
        // Optional marker (@cert-authority, @revoked) before the host list.
        let mut fields = line.split_whitespace();
        let mut first = fields.next().unwrap_or("");
        if first.starts_with('@') {
            first = fields.next().unwrap_or("");
        }
        for entry in first.split(',') {
            if entry.starts_with('|') || entry.contains(['*', '?', '!']) || entry.is_empty() {
                continue;
            }
            // [host]:port -> host
            let host = entry.strip_prefix('[').and_then(|e| e.split_once(']')).map(|(h, _)| h).unwrap_or(entry);
            if !host.is_empty() {
                out.push(host.to_string());
            }
        }
    }
    out
}

/// Tags used in `PREFIX-<tag>-<digits>` snapshot names (short names), sorted, without duplicates.
pub fn tags_from<'a>(prefix: &str, snapshots: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut tags = BTreeSet::new();
    for s in snapshots {
        let Some(rest) = s.strip_prefix(prefix).and_then(|r| r.strip_prefix('-')) else { continue };
        let Some((tag, stamp)) = rest.rsplit_once('-') else { continue };
        if !tag.is_empty() && !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit()) {
            tags.insert(tag.to_string());
        }
    }
    tags.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes() -> OptionShapes {
        let mut s = OptionShapes::default();
        s.add("destination-host", "D", true);
        s.add("source-host", "S", true);
        s.add("verbose", "v", false);
        s.add("force", "F", false);
        s.add("ssh-opt", "", true);
        s
    }

    fn w(words: &[&str]) -> Walk {
        walk(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &shapes())
    }

    #[test]
    fn positionals_skip_options_and_their_values() {
        assert_eq!(w(&[]).positionals, Vec::<String>::new());
        assert_eq!(w(&["tank/a"]).positionals, vec!["tank/a"]);
        assert_eq!(w(&["-D", "backup", "tank/a", "-v"]).positionals, vec!["tank/a"]);
        assert_eq!(w(&["--destination-host", "backup", "tank/a"]).positionals, vec!["tank/a"]);
        assert_eq!(w(&["--destination-host=backup", "tank/a"]).positionals, vec!["tank/a"]);
        assert_eq!(w(&["-Dbackup", "tank/a"]).positionals, vec!["tank/a"]);
        assert_eq!(w(&["-vF", "tank/a"]).positionals, vec!["tank/a"]);
        assert_eq!(w(&["-vD", "backup", "tank/a"]).positionals, vec!["tank/a"]);
        assert_eq!(w(&["--", "-weird"]).positionals, vec!["-weird"]);
        assert_eq!(w(&["--ssh-opt", "A=1", "x", "y"]).positionals, vec!["x", "y"]);
    }

    #[test]
    fn pending_option_value() {
        let r = w(&["tank/a", "-D"]);
        assert_eq!(r.pending.as_deref(), Some("destination-host"));
        assert_eq!(r.parseable, vec!["tank/a"]);
        let r = w(&["-vS"]);
        assert_eq!(r.pending.as_deref(), Some("source-host"));
        assert!(r.parseable.is_empty());
        let r = w(&["--source-host"]);
        assert_eq!(r.pending.as_deref(), Some("source-host"));
        let r = w(&["-D", "backup"]);
        assert_eq!(r.pending, None);
        assert_eq!(r.parseable, vec!["-D", "backup"]);
        assert_eq!(w(&["--verbose"]).pending, None);
    }

    #[test]
    fn ssh_config_hosts() {
        let text = "Host backup nas\n  HostName 10.0.0.2\nHost *.lan !bad web?\nhost=offsite\n# Host commented\nMatch host x\n";
        assert_eq!(config_hosts(text), vec!["backup", "nas", "offsite"]);
    }

    #[test]
    fn known_hosts_entries() {
        let text = "backup,10.0.0.2 ssh-ed25519 AAAA\n[nas]:2222 ssh-rsa AAAA\n|1|abc=|def= ssh-ed25519 AAAA\n\
                    @cert-authority *.lan ssh-rsa AAAA\n# comment\n\nweb ecdsa AAAA\n";
        assert_eq!(known_hosts(text), vec!["backup", "10.0.0.2", "nas", "web"]);
    }

    #[test]
    fn hosts_from_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".ssh/config"), "Host backup\n").unwrap();
        std::fs::write(dir.path().join(".ssh/known_hosts"), "backup ssh-rsa A\nnas ssh-rsa A\n").unwrap();
        assert_eq!(ssh_hosts(dir.path()), vec!["backup", "nas"]);
        assert!(ssh_hosts(Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn tags() {
        let names = ["zsync-hourly-1", "zsync-hourly-2", "zsync-daily-20240101", "zsync-20240101", "zsync-a-b-3", "manual", "zsync-x-y"];
        assert_eq!(tags_from("zsync", names), vec!["a-b", "daily", "hourly"]);
        assert_eq!(tags_from("auto", ["auto-weekly-1", "zsync-hourly-1"]), vec!["weekly"]);
    }
}
