//! An `ssh` stand-in for tests: runs the remote command locally with `sh -c`, pointing the fake
//! `zfs` at a per-host state file (`$FAKEZFS_ROOT/<host>.json`). Every invocation is appended to
//! `$FAKESSH_LOG` when set, so tests can check the ssh options zsync passed. The host
//! `unreachable` fails like a DNS error (exit 255).

use std::io::Write;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(log) = std::env::var("FAKESSH_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            let _ = writeln!(f, "{}", args.join(" "));
        }
    }
    // OpenSSH options that take a value.
    const WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut i = 0;
    let mut user = None;
    while i < args.len() && args[i].starts_with('-') {
        let a = &args[i];
        let c = a.chars().nth(1).unwrap_or(' ');
        if WITH_ARG.contains(c) {
            let v = if a.len() > 2 {
                a[2..].to_string()
            } else {
                i += 1;
                args.get(i).cloned().unwrap_or_default()
            };
            if c == 'l' {
                user = Some(v);
            }
        }
        i += 1;
    }
    let Some(dest) = args.get(i) else {
        eprintln!("usage: ssh [options] host command");
        std::process::exit(255);
    };
    let host = dest.rsplit_once('@').map(|(_, h)| h).unwrap_or(dest).to_string();
    let _ = user;
    if host == "unreachable" {
        eprintln!("ssh: Could not resolve hostname {host}: Name or service not known");
        std::process::exit(255);
    }
    let command = args[i + 1..].join(" ");
    if command.is_empty() {
        eprintln!("fake-ssh: interactive sessions are not supported");
        std::process::exit(255);
    }
    let root = std::env::var("FAKEZFS_ROOT").unwrap_or_else(|_| {
        eprintln!("fake-ssh: FAKEZFS_ROOT is not set");
        std::process::exit(255)
    });
    let status = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .env("FAKEZFS_STATE", format!("{root}/{host}.json"))
        .env("FAKE_SSH_HOST", &host)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("fake-ssh: {e}");
            std::process::exit(255)
        });
    std::process::exit(status.code().unwrap_or(255));
}
