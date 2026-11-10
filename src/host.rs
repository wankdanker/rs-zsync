//! Where a command runs: locally, or on a remote host through OpenSSH.
//!
//! Commands are described as a [`Script`]: either an argv (run directly, or shell-quoted for the
//! remote shell) or a POSIX shell script (run with `sh -c`, which is also how remote scripts are
//! wrapped so they behave the same whatever the remote login shell is).

use crate::util::{shell_join, shell_quote};
use std::sync::Arc;

/// Settings shared by every ssh connection zsync opens.
#[derive(Clone, Debug)]
pub struct SshConfig {
    /// ssh program and leading arguments, e.g. `["ssh"]` (overridable with `ZSYNC_SSH`/`--ssh-program`).
    pub program: Vec<String>,
    pub user: Option<String>,
    pub key: Option<String>,
    pub port: Option<u16>,
    /// Extra `-o` options, e.g. `Compression=no`.
    pub options: Vec<String>,
    /// Reuse one connection per host (ControlMaster) for the many small `zfs list` calls.
    pub multiplex: bool,
}

impl Default for SshConfig {
    fn default() -> Self {
        SshConfig { program: vec!["ssh".into()], user: None, key: None, port: None, options: Vec::new(), multiplex: false }
    }
}

impl SshConfig {
    /// The ssh argv up to and including the host, without the remote command.
    pub fn argv_for_host(&self, host: &str) -> Vec<String> {
        let mut argv = self.program.clone();
        if let Some(user) = self.user.as_ref().filter(|u| !u.is_empty() && !host.contains('@')) {
            argv.push("-l".into());
            argv.push(user.clone());
        }
        if let Some(key) = self.key.as_ref().filter(|k| !k.is_empty()) {
            argv.push("-i".into());
            argv.push(key.clone());
        }
        if let Some(port) = self.port {
            argv.push("-p".into());
            argv.push(port.to_string());
        }
        for opt in &self.options {
            argv.push("-o".into());
            argv.push(opt.clone());
        }
        if self.multiplex {
            for opt in ["ControlMaster=auto", "ControlPath=~/.ssh/zsync-%C", "ControlPersist=60"] {
                argv.push("-o".into());
                argv.push(opt.into());
            }
        }
        argv.push(host.to_string());
        argv
    }
}

/// What to run.
#[derive(Clone, Debug)]
pub enum Script {
    Argv(Vec<String>),
    Shell(String),
}

impl Script {
    /// The command as one line of POSIX shell.
    pub fn to_shell(&self) -> String {
        match self {
            Script::Argv(argv) => shell_join(argv),
            Script::Shell(s) => s.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Host {
    /// `None` for the local machine, else the ssh destination (`host` or `user@host`).
    pub name: Option<String>,
    pub ssh: Arc<SshConfig>,
}

impl Host {
    /// A host from an option value: empty means local, as in node-zsync.
    pub fn new(name: Option<&str>, ssh: Arc<SshConfig>) -> Host {
        Host { name: name.map(str::trim).filter(|n| !n.is_empty()).map(String::from), ssh }
    }

    pub fn local(ssh: Arc<SshConfig>) -> Host {
        Host { name: None, ssh }
    }

    pub fn is_local(&self) -> bool {
        self.name.is_none()
    }

    /// `local` or the host name, for messages and status output.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("local")
    }

    /// The argv to spawn on this machine that runs `script` on this host.
    pub fn argv_for(&self, script: &Script) -> Vec<String> {
        match (&self.name, script) {
            (None, Script::Argv(argv)) => argv.clone(),
            (None, Script::Shell(s)) => vec!["sh".into(), "-c".into(), s.clone()],
            (Some(host), Script::Argv(argv)) => {
                let mut v = self.ssh.argv_for_host(host);
                v.push(shell_join(argv));
                v
            }
            (Some(host), Script::Shell(s)) => {
                let mut v = self.ssh.argv_for_host(host);
                v.push(format!("sh -c {}", shell_quote(s)));
                v
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh() -> Arc<SshConfig> {
        Arc::new(SshConfig {
            program: vec!["ssh".into()],
            user: Some("root".into()),
            key: Some("/k".into()),
            port: Some(2222),
            options: vec!["Compression=no".into()],
            multiplex: false,
        })
    }

    #[test]
    fn local_commands() {
        let h = Host::new(Some(""), ssh());
        assert!(h.is_local());
        assert_eq!(h.label(), "local");
        assert_eq!(h.argv_for(&Script::Argv(vec!["zfs".into(), "list".into()])), vec!["zfs", "list"]);
        assert_eq!(h.argv_for(&Script::Shell("a | b".into())), vec!["sh", "-c", "a | b"]);
    }

    #[test]
    fn remote_commands() {
        let h = Host::new(Some("backup"), ssh());
        assert_eq!(
            h.argv_for(&Script::Argv(vec!["zfs".into(), "list".into(), "pool/my data".into()])),
            vec!["ssh", "-l", "root", "-i", "/k", "-p", "2222", "-o", "Compression=no", "backup", "zfs list 'pool/my data'"]
        );
        let argv = h.argv_for(&Script::Shell("a | b".into()));
        assert_eq!(argv.last().unwrap(), "sh -c 'a | b'");
    }

    #[test]
    fn explicit_user_in_host_wins() {
        let h = Host::new(Some("admin@backup"), ssh());
        let argv = h.argv_for(&Script::Argv(vec!["true".into()]));
        assert!(!argv.contains(&"-l".to_string()));
        assert!(argv.contains(&"admin@backup".to_string()));
    }

    #[test]
    fn multiplexing() {
        let cfg = SshConfig { multiplex: true, ..Default::default() };
        let argv = cfg.argv_for_host("h");
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        assert_eq!(argv.last().unwrap(), "h");
    }
}
