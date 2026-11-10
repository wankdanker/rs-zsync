//! Moving a send stream from the source host to the destination host.
//!
//! * **relay** (default, as in node-zsync): `ssh src zfs send` → this process → `ssh dst zfs recv`.
//!   Works whenever this machine can reach both hosts; zsync counts bytes, limits bandwidth and
//!   reports progress itself.
//! * **direct**: the source host pipes into its own ssh to the destination
//!   (`ssh src 'zfs send | ssh dst zfs recv'`), so data skips this machine. The source needs ssh
//!   access to the destination (keys, agent forwarding via `--ssh-opt ForwardAgent=yes`, ...).
//! * **tcp**: the destination listens on a TCP port (`nc`, `ncat`, `socat` or `zsync net`) and
//!   the source connects to it. Control stays on ssh; the bulk stream is unencrypted, so use it
//!   only on trusted networks.
//!
//! Each side can add filter stages: `--compress` (zstd/lz4/gzip on both ends), `--send-pipe` and
//! `--receive-pipe` (e.g. `mbuffer -q -m 1G`), and on non-relay paths `--bwlimit` uses `pv -L`.

use crate::backend::Backend;
use crate::exec::{pipeline_script, spawn_pipeline, Finished, Io, LineSink, Running, Stage};
use crate::host::{Host, Script};
use crate::util::{bare_host, human_bytes, shell_join};
use anyhow::{bail, Result};
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Relay,
    Direct,
    Tcp,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode> {
        Ok(match s {
            "" | "relay" => Mode::Relay,
            "direct" => Mode::Direct,
            "tcp" => Mode::Tcp,
            _ => bail!("unknown transport '{s}' (expected relay, direct or tcp)"),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpTool {
    Nc,
    Ncat,
    Socat,
    Zsync,
}

impl TcpTool {
    pub fn parse(s: &str) -> Result<TcpTool> {
        Ok(match s {
            "" | "nc" => TcpTool::Nc,
            "ncat" => TcpTool::Ncat,
            "socat" => TcpTool::Socat,
            "zsync" => TcpTool::Zsync,
            _ => bail!("unknown tcp tool '{s}' (expected nc, ncat, socat or zsync)"),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Zstd,
    Lz4,
    Gzip,
}

impl Compression {
    pub fn parse(s: &str) -> Result<Compression> {
        Ok(match s {
            "" | "none" => Compression::None,
            "zstd" => Compression::Zstd,
            "lz4" => Compression::Lz4,
            "gzip" => Compression::Gzip,
            _ => bail!("unknown compression '{s}' (expected none, zstd, lz4 or gzip)"),
        })
    }

    fn filters(self) -> Option<(&'static str, &'static str)> {
        match self {
            Compression::None => None,
            Compression::Zstd => Some(("zstd -c -q -T0", "zstd -d -c -q")),
            Compression::Lz4 => Some(("lz4 -c -q", "lz4 -d -c -q")),
            Compression::Gzip => Some(("gzip -c", "gzip -d -c")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TcpOptions {
    pub tool: TcpTool,
    pub port: u16,
    /// Address the listener binds to (default: all).
    pub bind: Option<String>,
    /// Address the source connects to (default: the destination's host name).
    pub connect_host: Option<String>,
    pub ready_timeout: Duration,
}

impl Default for TcpOptions {
    fn default() -> Self {
        TcpOptions { tool: TcpTool::Nc, port: 9091, bind: None, connect_host: None, ready_timeout: Duration::from_secs(20) }
    }
}

/// How streams travel; shared by every dataset in a run.
#[derive(Clone, Debug)]
pub struct DataPath {
    pub mode: Mode,
    pub tcp: TcpOptions,
    pub compression: Compression,
    pub send_pipes: Vec<String>,
    pub recv_pipes: Vec<String>,
    /// Bytes per second.
    pub bwlimit: Option<u64>,
}

impl Default for DataPath {
    fn default() -> Self {
        DataPath { mode: Mode::Relay, tcp: TcpOptions::default(), compression: Compression::None, send_pipes: vec![], recv_pipes: vec![], bwlimit: None }
    }
}

impl DataPath {
    /// Whether progress must come from `zfs send -v -P` (zsync doesn't see the bytes).
    pub fn needs_send_progress(&self) -> bool {
        self.mode != Mode::Relay
    }
}

/// A progress report for one transfer.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProgressEvent {
    pub dataset: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub elapsed: f64,
    pub rate: f64,
    pub done: bool,
}

pub type ProgressSink = Arc<dyn Fn(&ProgressEvent) + Send + Sync>;

/// One stream to move.
pub struct Transfer<'a> {
    pub source: &'a Host,
    pub destination: &'a Host,
    /// The send stage argv (from [`Backend::send_stage`]).
    pub send: Vec<String>,
    /// The receive stage argv (from [`Backend::recv_stage`]).
    pub recv: Vec<String>,
    pub path: &'a DataPath,
    pub backend: &'a Backend,
    /// Name used in progress and errors (the destination dataset).
    pub label: String,
    pub estimate: Option<u64>,
    pub progress: Option<ProgressSink>,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub bytes: u64,
    pub seconds: f64,
}

fn forwarder(prefix: String) -> LineSink {
    Arc::new(move |line: &str| {
        let t = line.trim();
        if t.is_empty() {
            return;
        }
        let noisy = crate::zfs::parse_progress_line(t).is_some()
            || t.starts_with("full\t")
            || t.starts_with("incremental\t")
            || t.starts_with("size\t")
            || t.starts_with("resume token contents");
        if noisy {
            crate::debug!("[{prefix}] {t}");
        } else {
            crate::verbose!("[{prefix}] {t}");
        }
    })
}

/// Sink that turns `zfs send -v` progress lines into a cumulative byte count. With `-I` each
/// snapshot's counter restarts, so finished snapshots are added to a base.
fn progress_parser(counter: Arc<AtomicU64>, next: LineSink) -> LineSink {
    let state: Arc<Mutex<(String, u64, u64)>> = Arc::new(Mutex::new((String::new(), 0, 0)));
    Arc::new(move |line: &str| {
        if let Some((bytes, snap)) = crate::zfs::parse_progress_line(line) {
            let mut s = state.lock().unwrap();
            if s.0 != snap {
                s.1 += s.2;
                s.0 = snap;
            }
            s.2 = bytes;
            counter.store(s.1 + s.2, Ordering::Relaxed);
        }
        next(line);
    })
}

/// Sink that also flags when a TCP listener reports it is ready.
fn readiness(flag: Arc<(Mutex<bool>, Condvar)>, next: LineSink) -> LineSink {
    Arc::new(move |line: &str| {
        if line.to_ascii_lowercase().contains("listening") {
            let (m, c) = &*flag;
            *m.lock().unwrap() = true;
            c.notify_all();
        }
        next(line);
    })
}

fn tcp_listen_stage(tool: TcpTool, bind: Option<&str>, port: u16, zsync: &[String]) -> String {
    match tool {
        TcpTool::Nc => match bind {
            Some(b) => format!("nc -v -l {} {port}", shell_join(&[b])),
            None => format!("nc -v -l {port}"),
        },
        TcpTool::Ncat => match bind {
            Some(b) => format!("ncat -v --recv-only -l {} {port}", shell_join(&[b])),
            None => format!("ncat -v --recv-only -l {port}"),
        },
        TcpTool::Socat => {
            let bind = bind.map(|b| format!(",bind={b}")).unwrap_or_default();
            format!("socat -d -d -u {} STDOUT", shell_join(&[format!("TCP-LISTEN:{port},reuseaddr{bind}")]))
        }
        TcpTool::Zsync => {
            let mut argv = zsync.to_vec();
            argv.extend(["net".into(), "listen".into(), format!("{}:{port}", bind.unwrap_or("0.0.0.0"))]);
            shell_join(&argv)
        }
    }
}

fn tcp_connect_stage(tool: TcpTool, host: &str, port: u16, zsync: &[String]) -> String {
    match tool {
        TcpTool::Nc => format!("nc -N {} {port}", shell_join(&[host])),
        TcpTool::Ncat => format!("ncat --send-only {} {port}", shell_join(&[host])),
        TcpTool::Socat => format!("socat -u STDIN {}", shell_join(&[format!("TCP:{host}:{port},retry=50,interval=0.2")])),
        TcpTool::Zsync => {
            let mut argv = zsync.to_vec();
            argv.extend(["net".into(), "connect".into(), format!("{host}:{port}")]);
            shell_join(&argv)
        }
    }
}

/// Token-bucket style limiter: sleeps so the average rate stays under `rate` bytes/second.
pub struct RateLimiter {
    rate: u64,
    start: Instant,
    sent: u64,
}

impl RateLimiter {
    pub fn new(rate: u64) -> RateLimiter {
        RateLimiter { rate: rate.max(1), start: Instant::now(), sent: 0 }
    }
    pub fn delay_for(&mut self, n: u64) -> Duration {
        self.sent += n;
        let due = Duration::from_secs_f64(self.sent as f64 / self.rate as f64);
        due.saturating_sub(self.start.elapsed())
    }
    pub fn throttle(&mut self, n: u64) {
        let d = self.delay_for(n);
        if !d.is_zero() {
            std::thread::sleep(d);
        }
    }
}

/// Copy until EOF or a write error (the receiver went away). Returns bytes copied and whether
/// the copy stopped because writing failed.
pub fn relay_copy(mut src: impl Read, mut dst: impl Write, limit: Option<u64>, counter: &AtomicU64) -> (u64, bool) {
    let mut buf = vec![0u8; 256 * 1024];
    let mut limiter = limit.map(RateLimiter::new);
    let mut total = 0u64;
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if dst.write_all(&buf[..n]).is_err() {
            return (total, true);
        }
        total += n as u64;
        counter.store(total, Ordering::Relaxed);
        if let Some(l) = limiter.as_mut() {
            l.throttle(n as u64);
        }
    }
    let _ = dst.flush();
    (total, false)
}

struct Ticker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn start_ticker(label: String, counter: Arc<AtomicU64>, total: Option<u64>, sink: Option<ProgressSink>, started: Instant) -> Ticker {
    let stop = Arc::new(AtomicBool::new(false));
    let handle = sink.map(|sink| {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                if last.elapsed() >= Duration::from_secs(1) {
                    last = Instant::now();
                    sink(&event(&label, counter.load(Ordering::Relaxed), total, started, false));
                }
            }
        })
    });
    Ticker { stop, handle }
}

fn event(label: &str, bytes: u64, total: Option<u64>, started: Instant, done: bool) -> ProgressEvent {
    let elapsed = started.elapsed().as_secs_f64();
    ProgressEvent { dataset: label.into(), bytes, total, elapsed, rate: if elapsed > 0.0 { bytes as f64 / elapsed } else { 0.0 }, done }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Human progress line for stderr.
pub fn human_progress(e: &ProgressEvent) -> String {
    let pct = e.total.filter(|t| *t > 0).map(|t| format!(" {:>3}%", (e.bytes.saturating_mul(100) / t).min(100))).unwrap_or_default();
    let of = e.total.map(|t| format!(" / {}", human_bytes(t))).unwrap_or_default();
    format!("{}: {}{of}{pct} at {}/s{}", e.dataset, human_bytes(e.bytes), human_bytes(e.rate as u64), if e.done { " done" } else { "" })
}

fn send_stages(t: &Transfer) -> Vec<Stage> {
    let mut stages = vec![Stage::argv("send", &t.send)];
    if let Some((c, _)) = t.path.compression.filters() {
        stages.push(Stage::shell("compress", c));
    }
    for (i, p) in t.path.send_pipes.iter().enumerate() {
        stages.push(Stage::shell(&format!("send-pipe-{}", i + 1), p));
    }
    if t.path.mode != Mode::Relay {
        if let Some(rate) = t.path.bwlimit {
            stages.push(Stage::shell("bwlimit", &format!("pv -q -L {rate}")));
        }
    }
    stages
}

fn recv_stages(t: &Transfer) -> Vec<Stage> {
    let mut stages = Vec::new();
    for (i, p) in t.path.recv_pipes.iter().enumerate() {
        stages.push(Stage::shell(&format!("receive-pipe-{}", i + 1), p));
    }
    if let Some((_, d)) = t.path.compression.filters() {
        stages.push(Stage::shell("decompress", d));
    }
    stages.push(Stage::argv("receive", &t.recv));
    stages
}

/// A stream failed to move (as opposed to a planning error); `--retries` retries only these.
#[derive(Debug, Clone)]
pub struct TransferError(pub String);

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransferError {}

/// Combine the two sides' results: receive problems first (the usual root cause), then send.
fn conclude(label: &str, send: &Finished, recv: &Finished) -> Result<()> {
    if send.ok() && recv.ok() {
        return Ok(());
    }
    let mut parts = Vec::new();
    if !recv.ok() {
        parts.push(format!("receive: {}", recv.error_message()));
    }
    if !send.ok() {
        parts.push(format!("send: {}", send.error_message()));
    }
    Err(TransferError(format!("transfer to {label} failed: {}", parts.join(" | "))).into())
}

/// Move one stream according to `t.path`.
pub fn transfer(t: &Transfer) -> Result<Stats> {
    let started = Instant::now();
    let counter = Arc::new(AtomicU64::new(0));
    let ticker = start_ticker(t.label.clone(), counter.clone(), t.estimate, t.progress.clone(), started);
    let send_sink = forwarder(format!("send {}", t.source.label()));
    let send_sink = if t.path.needs_send_progress() { progress_parser(counter.clone(), send_sink) } else { send_sink };
    let recv_sink = forwarder(format!("receive {}", t.destination.label()));

    let mode = match t.path.mode {
        Mode::Direct if t.destination.is_local() && t.source.is_local() => Mode::Relay,
        Mode::Direct if t.destination.is_local() => {
            bail!("the direct transport needs a remote destination (the source would have to ssh back here); use relay or tcp")
        }
        m => m,
    };

    let result = match mode {
        Mode::Relay => {
            let mut recv = spawn_pipeline(t.destination, &recv_stages(t), Io::Piped, Io::Piped, true, recv_sink)?;
            let mut send = match spawn_pipeline(t.source, &send_stages(t), Io::Null, Io::Piped, false, send_sink) {
                Ok(s) => s,
                Err(e) => {
                    recv.kill();
                    let _ = recv.wait();
                    return Err(e);
                }
            };
            let out = send.take_stdout().expect("piped");
            let inp = recv.take_stdin().expect("piped");
            let (bytes, write_failed) = relay_copy(out, inp, t.path.bwlimit, &counter);
            if write_failed {
                crate::debug!("relay: receiver closed its input after {bytes} bytes");
            }
            let s = send.wait();
            let r = recv.wait();
            conclude(&t.label, &s, &r)
        }
        Mode::Direct => {
            let recv_script = pipeline_script(&recv_stages(t));
            let ssh = t.destination.argv_for(&Script::Shell(recv_script));
            let mut stages = send_stages(t);
            stages.push(Stage::argv("ssh-destination", &ssh));
            let r = spawn_pipeline(t.source, &stages, Io::Null, Io::Null, false, send_sink)?.wait();
            // The nested receive stages report through the same stderr as "extra" codes.
            let expected: Vec<String> = recv_stages(t).iter().map(|s| s.label.clone()).collect();
            let missing: Vec<&String> = expected.iter().filter(|l| r.code_of(l).is_none()).collect();
            if r.ok() && missing.is_empty() {
                Ok(())
            } else {
                let mut msg = r.error_message();
                if !missing.is_empty() {
                    msg.push_str(&format!(" (no status from {})", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
                }
                Err(TransferError(format!("transfer to {} failed: {msg}", t.label)).into())
            }
        }
        Mode::Tcp => tcp_transfer(t, send_sink, recv_sink),
    };

    let bytes = counter.load(Ordering::Relaxed);
    drop(ticker);
    if let Some(sink) = &t.progress {
        if result.is_ok() {
            sink(&event(&t.label, bytes, t.estimate, started, true));
        }
    }
    result.map(|_| Stats { bytes, seconds: started.elapsed().as_secs_f64() })
}

fn tcp_transfer(t: &Transfer, send_sink: LineSink, recv_sink: LineSink) -> Result<()> {
    let opts = &t.path.tcp;
    let connect_host = match (&opts.connect_host, &t.destination.name) {
        (Some(h), _) => h.clone(),
        (None, Some(h)) => bare_host(h).to_string(),
        (None, None) if t.source.is_local() => "127.0.0.1".into(),
        (None, None) => bail!("the tcp transport to a local destination from a remote source needs --tcp-host (this machine's address as the source sees it)"),
    };
    let ready = Arc::new((Mutex::new(false), Condvar::new()));
    let mut recv = recv_stages(t);
    recv.insert(0, Stage::shell("listen", &tcp_listen_stage(opts.tool, opts.bind.as_deref(), opts.port, &t.backend.zsync_words(t.destination))));
    let mut listener: Running = spawn_pipeline(t.destination, &recv, Io::Null, Io::Piped, true, readiness(ready.clone(), recv_sink))?;

    // Wait for the listener to say it is listening (or to die).
    let deadline = Instant::now() + opts.ready_timeout;
    loop {
        let (m, c) = &*ready;
        let guard = m.lock().unwrap();
        if *guard {
            break;
        }
        let (guard, _) = c.wait_timeout(guard, Duration::from_millis(100)).unwrap();
        if *guard {
            break;
        }
        drop(guard);
        if listener.has_exited() {
            let r = listener.wait();
            bail!("tcp listener on {} exited before accepting a connection: {}", t.destination.label(), r.error_message());
        }
        if Instant::now() > deadline {
            listener.kill();
            let r = listener.wait();
            bail!("tcp listener on {} did not report readiness within {:?}: {}", t.destination.label(), opts.ready_timeout, r.error_message());
        }
    }

    let mut send = send_stages(t);
    send.push(Stage::shell("connect", &tcp_connect_stage(opts.tool, &connect_host, opts.port, &t.backend.zsync_words(t.source))));
    let sender = match spawn_pipeline(t.source, &send, Io::Null, Io::Null, false, send_sink) {
        Ok(s) => s,
        Err(e) => {
            listener.kill();
            let _ = listener.wait();
            return Err(e);
        }
    };
    let s = sender.wait();
    if !s.ok() {
        // If the sender never connected, the listener would wait forever.
        let grace = Instant::now() + Duration::from_secs(5);
        while !listener.has_exited() && Instant::now() < grace {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !listener.has_exited() {
            listener.kill();
        }
    }
    let r = listener.wait();
    conclude(&t.label, &s, &r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SshConfig;

    fn local() -> Host {
        Host::local(Arc::new(SshConfig::default()))
    }

    #[test]
    fn parse_choices() {
        assert_eq!(Mode::parse("tcp").unwrap(), Mode::Tcp);
        assert!(Mode::parse("udp").is_err());
        assert_eq!(TcpTool::parse("socat").unwrap(), TcpTool::Socat);
        assert_eq!(Compression::parse("zstd").unwrap(), Compression::Zstd);
        assert!(Compression::parse("brotli").is_err());
    }

    #[test]
    fn tcp_stage_commands() {
        let z = vec!["/usr/bin/zsync".to_string()];
        assert_eq!(tcp_listen_stage(TcpTool::Nc, None, 9000, &z), "nc -v -l 9000");
        assert_eq!(tcp_listen_stage(TcpTool::Nc, Some("10.0.0.2"), 9000, &z), "nc -v -l 10.0.0.2 9000");
        assert_eq!(tcp_listen_stage(TcpTool::Socat, Some("10.0.0.2"), 9000, &z), "socat -d -d -u TCP-LISTEN:9000,reuseaddr,bind=10.0.0.2 STDOUT");
        assert_eq!(tcp_listen_stage(TcpTool::Zsync, None, 9000, &z), "/usr/bin/zsync net listen 0.0.0.0:9000");
        assert_eq!(tcp_connect_stage(TcpTool::Nc, "backup", 9000, &z), "nc -N backup 9000");
        assert_eq!(tcp_connect_stage(TcpTool::Ncat, "backup", 9000, &z), "ncat --send-only backup 9000");
        assert_eq!(tcp_connect_stage(TcpTool::Zsync, "backup", 9000, &z), "/usr/bin/zsync net connect backup:9000");
    }

    #[test]
    fn limiter_math() {
        let mut l = RateLimiter::new(1000);
        let d = l.delay_for(2000);
        assert!(d > Duration::from_millis(1900) && d <= Duration::from_secs(2));
    }

    #[test]
    fn relay_copy_counts_and_limits() {
        let data = vec![7u8; 300_000];
        let counter = AtomicU64::new(0);
        let mut out = Vec::new();
        let start = Instant::now();
        let (n, failed) = relay_copy(&data[..], &mut out, Some(1_000_000), &counter);
        assert_eq!(n, 300_000);
        assert!(!failed);
        assert_eq!(out, data);
        assert_eq!(counter.load(Ordering::Relaxed), 300_000);
        assert!(start.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn progress_parser_accumulates_across_snapshots() {
        let counter = Arc::new(AtomicU64::new(0));
        let sink = progress_parser(counter.clone(), Arc::new(|_| {}));
        sink("12:00:00\t100\tp/a@1");
        sink("12:00:01\t300\tp/a@1");
        sink("12:00:02\t50\tp/a@2");
        assert_eq!(counter.load(Ordering::Relaxed), 350);
        sink("not progress");
        assert_eq!(counter.load(Ordering::Relaxed), 350);
    }

    #[test]
    fn human_progress_format() {
        let e = ProgressEvent { dataset: "b/a".into(), bytes: 512 * 1024, total: Some(1024 * 1024), elapsed: 1.0, rate: 524288.0, done: false };
        assert_eq!(human_progress(&e), "b/a: 512.0 KiB / 1.0 MiB  50% at 512.0 KiB/s");
    }

    fn plain_transfer(path: &DataPath, send: &str, recv: &str) -> Result<Stats> {
        let h = local();
        let b = Backend::default();
        let t = Transfer {
            source: &h,
            destination: &h,
            send: vec!["sh".into(), "-c".into(), send.into()],
            recv: vec!["sh".into(), "-c".into(), recv.into()],
            path,
            backend: &b,
            label: "test".into(),
            estimate: None,
            progress: None,
        };
        transfer(&t)
    }

    #[test]
    fn relay_moves_bytes_through_filters() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let path = DataPath { compression: Compression::Gzip, send_pipes: vec!["cat".into()], recv_pipes: vec!["cat".into()], ..Default::default() };
        let stats = plain_transfer(&path, "head -c 100000 /dev/zero", &format!("cat > {}", out.display())).unwrap();
        assert_eq!(std::fs::metadata(&out).unwrap().len(), 100000);
        assert!(stats.bytes < 100000, "gzip should shrink zeros: {}", stats.bytes);
    }

    #[test]
    fn relay_reports_receive_failure_first() {
        let path = DataPath::default();
        let err = plain_transfer(&path, "head -c 1000000 /dev/zero", "head -c 10 >/dev/null; echo 'cannot receive: bad stream' >&2; exit 1").unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("transfer to test failed: receive:"), "{msg}");
        assert!(msg.contains("bad stream"), "{msg}");
    }

    #[test]
    fn relay_reports_send_failure() {
        let path = DataPath::default();
        let err = plain_transfer(&path, "echo 'send: no such snapshot' >&2; exit 2", "cat >/dev/null").unwrap_err();
        assert!(err.to_string().contains("send: send exited 2"), "{err}");
    }

    #[test]
    fn direct_with_local_ends_falls_back_to_relay() {
        let path = DataPath { mode: Mode::Direct, ..Default::default() };
        plain_transfer(&path, "echo hi", "cat >/dev/null").unwrap();
    }

    #[test]
    fn tcp_over_local_nc() {
        if crate::exec::run(&local(), &["sh".into(), "-c".into(), "command -v nc".into()]).is_err() {
            eprintln!("nc not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let port = 20000 + (std::process::id() % 20000) as u16;
        let path = DataPath { mode: Mode::Tcp, tcp: TcpOptions { port, ..Default::default() }, ..Default::default() };
        plain_transfer(&path, "head -c 500000 /dev/urandom", &format!("cat > {}", out.display())).unwrap();
        assert_eq!(std::fs::metadata(&out).unwrap().len(), 500000);
    }
}
