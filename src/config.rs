//! Presets and defaults.
//!
//! Two formats are read:
//!
//! * **JSON presets** (node-zsync compatible), `~/.zsync.json` and `/etc/zsync.json`:
//!   `{"preset1": {"glob": "pool1/*", "destinationHost": "backup", "replication": true}}`.
//!   Keys are camelCase option names.
//! * **Native** clyops-style line files, `~/.config/zsync/zsync.conf` and `/etc/zsync.conf`:
//!   `key=value` lines set defaults for every command; `name:key=value` lines define preset
//!   `name`. Keys are long option names (`destination-host`), `#` starts a comment and
//!   `@include PATH` reads another file in place (relative to the including file).
//!
//! `--config PATH` replaces the default search; a file whose first non-blank character is `{`
//! is JSON, anything else is native. A missing `--config` file is an error (exit 2).
//!
//! Files are read lowest priority first: /etc (JSON, then native), then home (JSON, then native).

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Option values keyed by long option name (`destination-host`). Repeated keys accumulate in
/// order (for list options); scalar options use the last value.
pub type Settings = BTreeMap<String, Vec<String>>;

#[derive(Debug, Default, Clone)]
pub struct Config {
    pub defaults: Settings,
    pub presets: BTreeMap<String, Settings>,
    pub files: Vec<PathBuf>,
}

/// `destinationHost` -> `destination-host`; already-kebab keys pass through.
pub fn kebab(key: &str) -> String {
    let mut out = String::new();
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else if c == '_' {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    out.trim_start_matches('-').to_string()
}

fn json_values(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Null => vec![],
        serde_json::Value::Bool(b) => vec![b.to_string()],
        serde_json::Value::Number(n) => vec![n.to_string()],
        serde_json::Value::String(s) => vec![s.clone()],
        // Arrays: node-zsync accepted arrays for glob/exclude; keep one value per element.
        serde_json::Value::Array(a) => a.iter().flat_map(json_values).collect(),
        serde_json::Value::Object(_) => vec![v.to_string()],
    }
}

impl Config {
    /// Merge `other` on top of `self`.
    fn overlay(&mut self, other: Config) {
        for (k, v) in other.defaults {
            self.defaults.insert(k, v);
        }
        for (name, settings) in other.presets {
            let entry = self.presets.entry(name).or_default();
            for (k, v) in settings {
                entry.insert(k, v);
            }
        }
        self.files.extend(other.files);
    }

    pub fn parse_json(text: &str, path: &Path) -> Result<Config> {
        let v: serde_json::Value = serde_json::from_str(text).with_context(|| format!("invalid JSON in {}", path.display()))?;
        let obj = v.as_object().with_context(|| format!("{}: expected an object of presets", path.display()))?;
        let mut cfg = Config { files: vec![path.to_path_buf()], ..Default::default() };
        for (name, body) in obj {
            let Some(body) = body.as_object() else { continue };
            let settings = cfg.presets.entry(name.clone()).or_default();
            for (k, v) in body {
                settings.insert(kebab(k), json_values(v));
            }
        }
        Ok(cfg)
    }

    pub fn parse_native(path: &Path) -> Result<Config> {
        let mut cfg = Config { files: vec![path.to_path_buf()], ..Default::default() };
        read_native(path, 0, &mut Vec::new(), &mut cfg)?;
        Ok(cfg)
    }

    /// Load one explicit file (JSON or native by content).
    pub fn load_file(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("config file not found: {}", path.display()))?;
        if text.trim_start().starts_with('{') {
            Config::parse_json(&text, path)
        } else {
            Config::parse_native(path)
        }
    }

    /// Load the default locations under `home` and `etc` (missing files are skipped).
    pub fn load_defaults(home: Option<&Path>, etc: &Path) -> Result<Config> {
        let mut candidates = vec![etc.join("zsync.json"), etc.join("zsync.conf")];
        if let Some(h) = home {
            candidates.push(h.join(".zsync.json"));
            candidates.push(h.join(".config/zsync/zsync.conf"));
        }
        let mut cfg = Config::default();
        for path in candidates.iter().filter(|p| p.is_file()) {
            cfg.overlay(Config::load_file(path)?);
        }
        Ok(cfg)
    }

    /// Settings for `preset` (if it exists) layered over the global defaults.
    pub fn resolve(&self, preset: Option<&str>) -> (Settings, bool) {
        let mut out = self.defaults.clone();
        let found = preset.and_then(|p| self.presets.get(p));
        if let Some(p) = found {
            for (k, v) in p {
                out.insert(k.clone(), v.clone());
            }
        }
        (out, found.is_some())
    }
}

fn read_native(path: &Path, depth: usize, stack: &mut Vec<PathBuf>, cfg: &mut Config) -> Result<()> {
    if depth > 10 {
        bail!("Config include depth exceeded (10) while processing: {}", path.display());
    }
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if stack.contains(&canon) {
        bail!("Circular config include detected: {}", path.display());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("config file not found: {}", path.display()))?;
    stack.push(canon);
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@include") {
            let target = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            read_native(&dir.join(target), depth + 1, stack, cfg)?;
            continue;
        }
        let Some((lhs, value)) = line.split_once('=') else { continue };
        let (preset, key) = match lhs.split_once(':') {
            Some((p, k)) => (Some(p.trim()), k.trim()),
            None => (None, lhs.trim()),
        };
        let key = kebab(key.trim_start_matches("--"));
        if key.is_empty() {
            continue;
        }
        let target = match preset {
            Some(p) if !p.is_empty() && p != "zsync" => cfg.presets.entry(p.to_string()).or_default(),
            _ => &mut cfg.defaults,
        };
        target.entry(key).or_default().push(value.trim().to_string());
    }
    stack.pop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_case() {
        assert_eq!(kebab("destinationHost"), "destination-host");
        assert_eq!(kebab("fallbackDestroyDestination"), "fallback-destroy-destination");
        assert_eq!(kebab("glob"), "glob");
        assert_eq!(kebab("lock_file"), "lock-file");
        assert_eq!(kebab("destination-host"), "destination-host");
    }

    #[test]
    fn json_presets_from_readme() {
        let text = r#"{"preset1": {"glob": "pool1/*", "exclude": "*/*swap", "destination": "pool3",
            "destinationHost": "someserver", "replication": true, "force": true, "destinationKeep": 2,
            "sshOpt": ["A=1", "B=2"]}}"#;
        let cfg = Config::parse_json(text, Path::new("x.json")).unwrap();
        let (s, found) = cfg.resolve(Some("preset1"));
        assert!(found);
        assert_eq!(s["glob"], vec!["pool1/*"]);
        assert_eq!(s["destination-host"], vec!["someserver"]);
        assert_eq!(s["replication"], vec!["true"]);
        assert_eq!(s["destination-keep"], vec!["2"]);
        assert_eq!(s["ssh-opt"], vec!["A=1", "B=2"]);
        let (_, found) = cfg.resolve(Some("pool1/*"));
        assert!(!found);
    }

    #[test]
    fn native_files_with_include() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("common.conf"), "ssh-opt=Compression=no\nuser=backup\n").unwrap();
        std::fs::write(
            dir.path().join("zsync.conf"),
            "# defaults\n@include common.conf\nuser=root\nzsync:verbose=true\n\
             nightly:glob=tank/*\nnightly:--destination=backup\nnightly:send-pipe=mbuffer -q\nnightly:send-pipe=cat\n",
        )
        .unwrap();
        let cfg = Config::load_file(&dir.path().join("zsync.conf")).unwrap();
        assert_eq!(cfg.defaults["user"], vec!["backup", "root"]);
        assert_eq!(cfg.defaults["ssh-opt"], vec!["Compression=no"]);
        assert_eq!(cfg.defaults["verbose"], vec!["true"]);
        let (s, found) = cfg.resolve(Some("nightly"));
        assert!(found);
        assert_eq!(s["glob"], vec!["tank/*"]);
        assert_eq!(s["destination"], vec!["backup"]);
        assert_eq!(s["send-pipe"], vec!["mbuffer -q", "cat"]);
        assert_eq!(s["user"].last().unwrap(), "root");
    }

    #[test]
    fn circular_include_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.conf"), "@include b.conf\n").unwrap();
        std::fs::write(dir.path().join("b.conf"), "@include a.conf\n").unwrap();
        let err = Config::load_file(&dir.path().join("a.conf")).unwrap_err();
        assert!(format!("{err:#}").contains("Circular config include"));
    }

    #[test]
    fn default_locations_layer() {
        let etc = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(etc.path().join("zsync.json"), r#"{"p": {"glob": "etc/*", "force": true}}"#).unwrap();
        std::fs::write(home.path().join(".zsync.json"), r#"{"p": {"glob": "home/*"}}"#).unwrap();
        std::fs::create_dir_all(home.path().join(".config/zsync")).unwrap();
        std::fs::write(home.path().join(".config/zsync/zsync.conf"), "verbose=true\n").unwrap();
        let cfg = Config::load_defaults(Some(home.path()), etc.path()).unwrap();
        let (s, _) = cfg.resolve(Some("p"));
        assert_eq!(s["glob"], vec!["home/*"]);
        assert_eq!(s["force"], vec!["true"]);
        assert_eq!(s["verbose"], vec!["true"]);
        assert_eq!(cfg.files.len(), 3);
    }

    #[test]
    fn missing_explicit_file() {
        assert!(Config::load_file(Path::new("/nonexistent/zsync.json")).is_err());
    }
}
