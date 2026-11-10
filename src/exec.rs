//! Running commands and pipelines on hosts.
//!
//! Metadata commands (`zfs list`, `zfs snapshot`, ...) run with [`run`], which captures output and
//! turns a non-zero exit into a [`CommandError`] carrying the tail of stderr.
//!
//! Data pipelines (`zfs send | zstd`, `nc -l | zfs receive`) run with [`spawn_pipeline`]. Every stage
//! is wrapped as `{ (cmd); echo "ZSYNC_RC:label:$?" >&2; }` so zsync learns each stage's exit status
//! even through ssh, nested ssh and shells without `pipefail`.

use crate::host::{Host, Script};
use crate::util::shell_join;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub const SENTINEL: &str = "ZSYNC_RC:";
const TAIL_LINES: usize = 8;

/// A command that ran and failed.
#[derive(Debug, Clone)]
pub struct CommandError {
    pub host: String,
    pub command: String,
    pub code: Option<i32>,
    pub stderr: String,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = summarize(&self.stderr);
        let code = self.code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
        if detail.is_empty() {
            write!(f, "command failed on {} (exit {code}): {}", self.host, self.command)
        } else {
            write!(f, "command failed on {} (exit {code}): {detail}", self.host)
        }
    }
}

impl std::error::Error for CommandError {}

/// Last few non-empty stderr lines joined with `; `.
pub fn summarize(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let start = lines.len().saturating_sub(5);
    lines[start..].join("; ")
}

/// Whether an error says the dataset does not exist (as `zfs list` reports it).
pub fn is_not_found(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("dataset does not exist")
}

fn command(host: &Host, script: &Script) -> (Command, String) {
    let argv = host.argv_for(script);
    let display = shell_join(&argv);
    crate::debug!("exec[{}]: {}", host.label(), display);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    (cmd, display)
}

/// Run an argv on `host`, returning stdout. Non-zero exit is a [`CommandError`].
pub fn run(host: &Host, argv: &[String]) -> Result<String> {
    run_script(host, &Script::Argv(argv.to_vec()), None)
}

/// Run a script on `host` with optional stdin bytes, returning stdout.
pub fn run_script(host: &Host, script: &Script, input: Option<&[u8]>) -> Result<String> {
    let (mut cmd, display) = command(host, script);
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| anyhow!("cannot run {display} on {}: {e}", host.label()))?;
    if let Some(data) = input {
        let mut stdin = child.stdin.take().expect("piped");
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let out = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !stderr.trim().is_empty() {
        crate::debug!("stderr[{}]: {}", host.label(), stderr.trim());
    }
    if !out.status.success() {
        return Err(CommandError { host: host.label().into(), command: display, code: out.status.code(), stderr }.into());
    }
    Ok(stdout)
}

/// Run a script on `host` and return (exit code, stdout, stderr) without judging the status.
pub fn capture(host: &Host, script: &Script) -> Result<(Option<i32>, String, String)> {
    let (mut cmd, display) = command(host, script);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().map_err(|e| anyhow!("cannot run {display} on {}: {e}", host.label()))?;
    Ok((out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
}

/// One stage of a pipeline: a shell snippet with a label used in its exit sentinel.
#[derive(Clone, Debug)]
pub struct Stage {
    pub label: String,
    pub shell: String,
}

impl Stage {
    pub fn argv(label: &str, argv: &[String]) -> Stage {
        Stage { label: label.into(), shell: shell_join(argv) }
    }
    pub fn shell(label: &str, snippet: &str) -> Stage {
        Stage { label: label.into(), shell: snippet.into() }
    }
}

/// `{ (a); echo "ZSYNC_RC:l1:$?" >&2; } | { (b); echo "ZSYNC_RC:l2:$?" >&2; }`. The subshell keeps
/// an `exit` in a user-supplied snippet from skipping its sentinel.
pub fn pipeline_script(stages: &[Stage]) -> String {
    stages.iter().map(|s| format!("{{ ({}); echo \"{SENTINEL}{}:$?\" >&2; }}", s.shell, s.label)).collect::<Vec<_>>().join(" | ")
}

/// Parse a sentinel line into (label, exit code).
pub fn parse_sentinel(line: &str) -> Option<(String, i32)> {
    let rest = line.trim().strip_prefix(SENTINEL)?;
    let (label, rc) = rest.rsplit_once(':')?;
    Some((label.to_string(), rc.trim().parse().ok()?))
}

/// Called with each stderr line of a pipeline that is not a sentinel.
pub type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default, Debug)]
struct StderrState {
    codes: HashMap<String, i32>,
    tail: VecDeque<String>,
}

/// A running pipeline.
pub struct Running {
    pub host: String,
    pub labels: Vec<String>,
    child: Child,
    state: Arc<Mutex<StderrState>>,
    reader: Option<JoinHandle<()>>,
    stdout_drain: Option<JoinHandle<()>>,
}

/// How a pipeline ended.
#[derive(Debug, Clone)]
pub struct Finished {
    pub host: String,
    pub status: Option<i32>,
    /// Exit code of each expected stage; `None` when its sentinel never arrived.
    pub stages: Vec<(String, Option<i32>)>,
    /// Exit codes reported for labels not in `stages` (e.g. a nested remote pipeline).
    pub extra: Vec<(String, i32)>,
    pub stderr_tail: Vec<String>,
}

impl Finished {
    pub fn ok(&self) -> bool {
        self.stages.iter().all(|(_, c)| *c == Some(0)) && self.extra.iter().all(|(_, c)| *c == 0)
    }

    /// Stages that failed (non-zero or missing status).
    pub fn failures(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .stages
            .iter()
            .filter(|(_, c)| *c != Some(0))
            .map(|(l, c)| match c {
                Some(code) => format!("{l} exited {code}"),
                None => format!("{l} did not finish"),
            })
            .collect();
        out.extend(self.extra.iter().filter(|(_, c)| *c != 0).map(|(l, c)| format!("{l} exited {c}")));
        out
    }

    pub fn code_of(&self, label: &str) -> Option<i32> {
        self.stages
            .iter()
            .find(|(l, _)| l == label)
            .and_then(|(_, c)| *c)
            .or_else(|| self.extra.iter().find(|(l, _)| l == label).map(|(_, c)| *c))
    }

    pub fn error_message(&self) -> String {
        let tail = self.stderr_tail.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ");
        let mut msg = format!("{} on {}", self.failures().join(", "), self.host);
        if self.status == Some(255) && self.stages.iter().all(|(_, c)| c.is_none()) {
            msg = format!("ssh to {} failed", self.host);
        }
        if !tail.is_empty() {
            msg.push_str(": ");
            msg.push_str(&tail);
        }
        msg
    }
}

/// Where a pipeline's stdin/stdout go.
pub enum Io {
    Null,
    Piped,
    Inherit,
}

impl Io {
    fn stdio(&self) -> Stdio {
        match self {
            Io::Null => Stdio::null(),
            Io::Piped => Stdio::piped(),
            Io::Inherit => Stdio::inherit(),
        }
    }
}

/// Spawn `stages` as one pipeline on `host`. stderr is always consumed: sentinels are recorded,
/// other lines go to `sink` and a short tail is kept for error messages. With `drain_stdout`,
/// stdout is read and fed to `sink` too (used for `zfs receive -v`).
pub fn spawn_pipeline(host: &Host, stages: &[Stage], stdin: Io, stdout: Io, drain_stdout: bool, sink: LineSink) -> Result<Running> {
    let script = Script::Shell(pipeline_script(stages));
    let (mut cmd, display) = command(host, &script);
    let drain = drain_stdout && matches!(stdout, Io::Piped);
    cmd.stdin(stdin.stdio()).stdout(stdout.stdio()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| anyhow!("cannot start pipeline on {}: {e}: {display}", host.label()))?;
    let state = Arc::new(Mutex::new(StderrState::default()));
    let stderr = child.stderr.take().expect("piped stderr");
    let st = state.clone();
    let sink2 = sink.clone();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).split(b'\n').map_while(|l| l.ok()) {
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            if let Some((label, code)) = parse_sentinel(line) {
                st.lock().unwrap().codes.insert(label, code);
                continue;
            }
            {
                let mut s = st.lock().unwrap();
                s.tail.push_back(line.to_string());
                while s.tail.len() > TAIL_LINES {
                    s.tail.pop_front();
                }
            }
            sink2(line);
        }
    });
    let stdout_drain = if drain {
        let out = child.stdout.take().expect("piped stdout");
        Some(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                sink(&line);
            }
        }))
    } else {
        None
    };
    Ok(Running {
        host: host.label().into(),
        labels: stages.iter().map(|s| s.label.clone()).collect(),
        child,
        state,
        reader: Some(reader),
        stdout_drain,
    })
}

impl Running {
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Whether the process has exited (without reaping status for callers).
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn wait(mut self) -> Finished {
        let status: Option<ExitStatus> = self.child.wait().ok();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stdout_drain.take() {
            let _ = h.join();
        }
        let st = self.state.lock().unwrap();
        let stages = self.labels.iter().map(|l| (l.clone(), st.codes.get(l).copied())).collect();
        let mut extra: Vec<(String, i32)> =
            st.codes.iter().filter(|(l, _)| !self.labels.contains(l)).map(|(l, c)| (l.clone(), *c)).collect();
        extra.sort();
        Finished {
            host: self.host.clone(),
            status: status.and_then(|s| s.code()),
            stages,
            extra,
            stderr_tail: st.tail.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SshConfig;

    fn local() -> Host {
        Host::local(Arc::new(SshConfig::default()))
    }

    fn quiet() -> LineSink {
        Arc::new(|_| {})
    }

    #[test]
    fn sentinel_parsing() {
        assert_eq!(parse_sentinel("ZSYNC_RC:send:0"), Some(("send".into(), 0)));
        assert_eq!(parse_sentinel("ZSYNC_RC:recv-filter-1:141\r"), Some(("recv-filter-1".into(), 141)));
        assert_eq!(parse_sentinel("hello"), None);
        assert_eq!(parse_sentinel("ZSYNC_RC:x:y"), None);
    }

    #[test]
    fn run_captures_and_errors() {
        let out = run(&local(), &["echo".into(), "hi".into()]).unwrap();
        assert_eq!(out, "hi\n");
        let err = run_script(&local(), &Script::Shell("echo boom >&2; exit 3".into()), None).unwrap_err();
        let ce = err.downcast_ref::<CommandError>().unwrap();
        assert_eq!(ce.code, Some(3));
        assert!(err.to_string().contains("boom"));
        let out = run_script(&local(), &Script::Shell("cat".into()), Some(b"data")).unwrap();
        assert_eq!(out, "data");
    }

    #[test]
    fn pipeline_reports_each_stage() {
        let stages =
            vec![Stage::shell("one", "printf abc"), Stage::shell("two", "tr a-z A-Z"), Stage::shell("three", "cat; echo oops >&2; exit 4")];
        let mut r = spawn_pipeline(&local(), &stages, Io::Null, Io::Piped, false, quiet()).unwrap();
        let mut out = String::new();
        std::io::Read::read_to_string(&mut r.take_stdout().unwrap(), &mut out).unwrap();
        let f = r.wait();
        assert_eq!(out, "ABC");
        assert_eq!(f.code_of("one"), Some(0));
        assert_eq!(f.code_of("two"), Some(0));
        assert_eq!(f.code_of("three"), Some(4));
        assert!(!f.ok());
        assert!(f.error_message().contains("three exited 4"));
        assert!(f.error_message().contains("oops"));
    }

    #[test]
    fn nested_sentinels_are_extra() {
        let stages = vec![Stage::shell("outer", "echo 'ZSYNC_RC:inner:2' >&2")];
        let f = spawn_pipeline(&local(), &stages, Io::Null, Io::Null, false, quiet()).unwrap().wait();
        assert_eq!(f.code_of("outer"), Some(0));
        assert_eq!(f.code_of("inner"), Some(2));
        assert!(!f.ok());
    }

    #[test]
    fn stdin_flows_through() {
        let stages = vec![Stage::shell("wc", "wc -c")];
        let mut r = spawn_pipeline(&local(), &stages, Io::Piped, Io::Piped, false, quiet()).unwrap();
        let mut stdin = r.take_stdin().unwrap();
        stdin.write_all(&[0u8; 1000]).unwrap();
        drop(stdin);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut r.take_stdout().unwrap(), &mut out).unwrap();
        assert!(r.wait().ok());
        assert_eq!(out.trim(), "1000");
    }
}
