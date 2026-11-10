//! `zsync send` / `zsync receive` (streams on stdout/stdin or a TCP socket) and the `zsync net`
//! helpers used by `--tcp-tool zsync`.

use crate::zfs::{self, RecvSpec, SendSpec};
use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn spawn(argv: &[String]) -> Command {
    crate::debug!("exec[local]: {}", crate::util::shell_join(argv));
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

fn check(status: std::process::ExitStatus, what: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{what} failed ({})", status.code().map(|c| format!("exit {c}")).unwrap_or_else(|| "signal".into()))
    }
}

/// Connect with retries until `timeout` (the listener may still be starting).
pub fn connect_retry(addr: &str, timeout: Duration) -> Result<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        let attempt = addr.to_socket_addrs().map_err(|e| anyhow!("cannot resolve {addr}: {e}")).and_then(|mut addrs| {
            let a = addrs.next().ok_or_else(|| anyhow!("cannot resolve {addr}"))?;
            TcpStream::connect_timeout(&a, Duration::from_secs(5)).map_err(|e| anyhow!("cannot connect to {addr}: {e}"))
        });
        match attempt {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Accept one connection on `addr`. Prints `zsync: listening on ADDR` to stderr once bound,
/// which the tcp transport waits for.
pub fn accept_one(addr: &str) -> Result<TcpStream> {
    let listener = TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
    eprintln!("zsync: listening on {}", listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| addr.into()));
    let (stream, peer) = listener.accept()?;
    crate::debug!("accepted connection from {peer}");
    Ok(stream)
}

/// `zsync send`: run `zfs send` locally, writing to stdout or to `connect`.
pub fn send(zfs_words: &[String], spec: &SendSpec, connect: Option<&str>) -> Result<()> {
    if spec.resume_token.is_none() && spec.snapshot.is_empty() {
        bail!("a snapshot to send is required (-s/--snapshot)");
    }
    let argv = zfs::send_argv(zfs_words, spec);
    match connect {
        None => check(spawn(&argv).stdin(Stdio::null()).status().context("cannot run zfs send")?, "zfs send"),
        Some(addr) => {
            let mut sock = connect_retry(addr, Duration::from_secs(30))?;
            let mut child = spawn(&argv).stdin(Stdio::null()).stdout(Stdio::piped()).spawn().context("cannot run zfs send")?;
            let mut out = child.stdout.take().expect("piped");
            let copied = std::io::copy(&mut out, &mut sock);
            drop(out);
            let _ = sock.shutdown(Shutdown::Write);
            let status = child.wait()?;
            copied.with_context(|| format!("sending to {addr}"))?;
            check(status, "zfs send")
        }
    }
}

/// `zsync receive`: run `zfs receive` locally, reading stdin or one connection on `listen`.
pub fn receive(zfs_words: &[String], spec: &RecvSpec, listen: Option<&str>) -> Result<()> {
    if spec.dataset.is_empty() {
        bail!("a destination dataset is required");
    }
    let argv = zfs::recv_argv(zfs_words, spec);
    match listen {
        None => check(spawn(&argv).status().context("cannot run zfs receive")?, "zfs receive"),
        Some(addr) => {
            let mut sock = accept_one(addr)?;
            let mut child = spawn(&argv).stdin(Stdio::piped()).spawn().context("cannot run zfs receive")?;
            let mut stdin = child.stdin.take().expect("piped");
            let copied = std::io::copy(&mut sock, &mut stdin);
            drop(stdin);
            let status = child.wait()?;
            check(status, "zfs receive")?;
            copied.map(|_| ()).context("reading the stream")
        }
    }
}

/// `zsync receive --abort`: discard partially received state.
pub fn receive_abort(zfs_words: &[String], dataset: &str) -> Result<()> {
    check(spawn(&zfs::recv_abort_argv(zfs_words, dataset)).status()?, "zfs receive -A")
}

/// `zsync net listen ADDR:PORT`: one connection's bytes to stdout.
pub fn net_listen(addr: &str) -> Result<u64> {
    let mut sock = accept_one(addr)?;
    let mut out = std::io::stdout().lock();
    let n = std::io::copy(&mut sock, &mut out)?;
    out.flush()?;
    Ok(n)
}

/// `zsync net connect HOST:PORT`: stdin to the connection.
pub fn net_connect(addr: &str) -> Result<u64> {
    let mut sock = connect_retry(addr, Duration::from_secs(30))?;
    let mut inp = std::io::stdin().lock();
    let n = std::io::copy(&mut inp, &mut sock)?;
    sock.shutdown(Shutdown::Write)?;
    // Wait for the peer to close so we don't exit before it has everything.
    let mut buf = [0u8; 1];
    let _ = sock.read(&mut buf);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_roundtrip_with_retry() {
        let port = 30000 + (std::process::id() % 20000) as u16;
        let addr = format!("127.0.0.1:{port}");
        let a2 = addr.clone();
        let client = std::thread::spawn(move || {
            let mut s = connect_retry(&a2, Duration::from_secs(10)).unwrap();
            s.write_all(b"hello").unwrap();
            s.shutdown(Shutdown::Write).unwrap();
        });
        std::thread::sleep(Duration::from_millis(300));
        let mut sock = accept_one(&addr).unwrap();
        let mut got = String::new();
        sock.read_to_string(&mut got).unwrap();
        client.join().unwrap();
        assert_eq!(got, "hello");
    }

    #[test]
    fn connect_gives_up() {
        let err = connect_retry("127.0.0.1:1", Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("cannot connect"));
    }
}
