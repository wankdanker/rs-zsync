//! The `zsync` command line, built on clyops.
//!
//! Options are registered per command, mirroring node-zsync's flags. Presets (JSON or native
//! config) are applied in a first pass: when the first positional names a preset, its settings
//! become option defaults through the environment clyops reads, so precedence is
//! command line > positional > preset > environment > default.

use clyops::{Cli, CompleteRequest, CompleteResult, Parsed, Values};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use zsync::backend::{serve_rpc, Backend, Mode as BackendMode};
use zsync::complete::{self as completion, OptionShapes};
use zsync::config::{Config, Settings};
use zsync::host::SshConfig;
use zsync::ops::{self, Ctx, Destination, Outcome, Selection};
use zsync::output::{json_err, json_ok, table};
use zsync::transport::{self, Compression, DataPath, ProgressEvent, ProgressSink, TcpOptions, TcpTool};
use zsync::util::{parse_size, split_words};
use zsync::zfs::{RecvSpec, RenameOpts, SendSpec};

const EXIT_ERROR: i32 = 1;
const EXIT_CONFIG: i32 = 2;
const EXIT_PARTIAL: i32 = 3;
/// 128 + SIGPIPE, what a shell reports for a process killed by a broken pipe.
const EXIT_BROKEN_PIPE: i32 = 141;

/// long option name -> (env/var name, is array), and each command's option shapes (for completion).
#[derive(Default)]
struct Registry {
    vars: HashMap<String, (String, bool)>,
    cmds: HashMap<String, OptionShapes>,
}

fn var_for(long: &str) -> String {
    match long {
        "lock-wait" => "LOCK_WAIT".into(),
        "ssh-program" => "ZSYNC_SSH".into(),
        "zfs-path" => "ZSYNC_ZFS".into(),
        _ => format!("ZSYNC_{}", long.to_ascii_uppercase().replace('-', "_")),
    }
}

struct Reg<'a> {
    cli: &'a mut Cli,
    reg: &'a mut Registry,
    cmd: &'static str,
}

impl Reg<'_> {
    /// Register an option: `default` is a value, `flag`, `optional` or "" (required).
    fn o(&mut self, long: &str, short: &str, default: &str, desc: &str, group: &str, rule: &str) -> &mut Self {
        let var = var_for(long);
        let h = self.cli.opt(&var, long, short, default, desc).group(group);
        if !rule.is_empty() {
            h.rule(rule);
        }
        self.reg.vars.insert(long.into(), (var, false));
        self.reg.cmds.entry(self.cmd.into()).or_default().add(long, short, default != "flag");
        self
    }
    fn a(&mut self, long: &str, desc: &str, group: &str) -> &mut Self {
        let var = var_for(long);
        self.cli.opt_array(&var, long, "", desc).group(group);
        self.reg.vars.insert(long.into(), (var, true));
        self.reg.cmds.entry(self.cmd.into()).or_default().add(long, "", true);
        self
    }
    fn ssh(&mut self) -> &mut Self {
        self.o("user", "u", "optional", "remote ssh user", "SSH", "")
            .o("key", "k", "optional", "path to ssh private key", "SSH", "")
            .o("ssh-port", "", "optional", "remote ssh port", "SSH", "port")
            .a("ssh-opt", "extra ssh -o option, repeatable (e.g. Compression=no)", "SSH")
            .o("ssh-program", "", "ssh", "ssh command to use", "SSH", "")
            .o("ssh-multiplex", "", "flag", "reuse one ssh connection per host (ControlMaster)", "SSH", "")
            .o("zfs-path", "", "zfs", "zfs command on every host (e.g. \"sudo -n zfs\")", "SSH", "")
            .o("remote-zsync", "", "flag", "run zsync on remote hosts (zsync rpc/send/receive) instead of plain zfs", "SSH", "")
            .o("remote-zsync-path", "", "zsync", "zsync command on remote hosts", "SSH", "")
            .o(
                "complete-remote",
                "",
                "true",
                "on Tab, list datasets on remote hosts over ssh (BatchMode); false keeps completion local",
                "SSH",
                "bool",
            )
    }
    fn select(&mut self, recursive: &str) -> &mut Self {
        self.o("type", "t", "optional", "filter file system types", "Selection", "")
            .o("glob", "g", "optional", "dataset-glob search glob, comma separated", "Selection", "")
            .o("exclude", "x", "optional", "exclude datasets by glob, comma separated", "Selection", "")
            .o("recursive", "R", "flag", recursive, "Selection", "")
            .o("source", "s", "optional", "source-dataset, eg: pool/vol1, pool", "Selection", "")
            .o("source-host", "S", "optional", "host on which the source dataset resides", "Selection", "")
    }
    fn dest(&mut self) -> &mut Self {
        self.o("destination", "d", "optional", "destination-base, eg: pool2/virtual-disks, pool2", "Destination", "")
            .o("destination-host", "D", "optional", "host on which the destination dataset resides", "Destination", "")
            .o(
                "destination-drop",
                "n",
                "optional",
                "[number] of elements to drop from the left side of [source-dataset]",
                "Destination",
                "int:0-",
            )
            .o(
                "destination-keep",
                "N",
                "optional",
                "[number] of elements to keep from the right side of [source-dataset]",
                "Destination",
                "int:0-",
            )
    }
    fn common(&mut self, config: bool) -> &mut Self {
        self.o("lock-file", "l", "optional", "lock file to use to prevent this command from running in another instance", "Control", "")
            .o("lock-wait", "w", "5000", "how long to wait for lock file (milliseconds)", "Control", "int:0-")
            .o("format", "f", "text", "output format", "Output", "choice:text,json")
            .o("verbose", "v", "flag", "verbose output", "Output", "")
            .o("debug", "V", "flag", "enable debug output (also DEBUG=zsync)", "Output", "");
        if config {
            self.o("config", "C", "optional", "path to presets config file, supersedes all other possible configs", "Output", "");
        }
        self
    }
    fn transfer(&mut self) -> &mut Self {
        self.o("latest", "L", "flag", "only send the latest snapshot when doing an incremental send", "Transfer", "")
            .o("force", "F", "flag", "force receive (may cause rollback)", "Transfer", "")
            .o("replication", "r", "flag", "enable a replication stream", "Transfer", "")
            .o("properties", "p", "flag", "include properties in the send stream", "Transfer", "")
            .o("compressed", "m", "flag", "send blocks compressed as they are on disk (zfs send -c)", "Transfer", "")
            .o("raw", "W", "flag", "for encrypted datasets, send data exactly as it exists on disk (zfs send -w)", "Transfer", "")
            .o("large-block", "", "flag", "allow blocks larger than 128K (zfs send -L)", "Transfer", "")
            .o("unmounted", "", "flag", "do not mount received file systems (zfs receive -u)", "Transfer", "")
            .o("continue", "c", "flag", "continue on to the next dataset if errors are encountered", "Transfer", "")
            .o(
                "fallback-rename-destination",
                "e",
                "flag",
                "if destination dataset exists and there is no common snapshot, then rename the remote dataset before send",
                "Transfer",
                "",
            )
            .o(
                "fallback-destroy-destination",
                "X",
                "flag",
                "if destination dataset exists and there is no common snapshot, then destroy the remote dataset before send",
                "Transfer",
                "",
            )
            .o("resumable", "", "true", "receive with -s and resume interrupted transfers (--no-resumable to disable)", "Transfer", "bool")
            .o("abort-resume", "", "flag", "discard partially received state instead of resuming it", "Transfer", "")
            .o("retries", "", "0", "retry a failed transfer this many times (resuming where possible)", "Transfer", "int:0-")
            .o(
                "bookmark",
                "",
                "flag",
                "bookmark each sent snapshot on the source so it stays an incremental base after rotation",
                "Transfer",
                "",
            )
            .o("prefix", "", "zsync", "snapshot name prefix zsync manages (for --bookmark cleanup)", "Transfer", "")
            .o("dry-run", "", "flag", "show what would be sent without sending", "Transfer", "")
            .o(
                "transport",
                "",
                "relay",
                "data path: relay (through this host), direct (source ssh to destination), tcp",
                "Transport",
                "choice:relay,direct,tcp",
            )
            .o("tcp-tool", "", "nc", "tcp listener/connector on the hosts", "Transport", "choice:nc,ncat,socat,zsync")
            .o("tcp-port", "", "9091", "tcp port the destination listens on", "Transport", "port")
            .o("tcp-bind", "", "optional", "address the destination listener binds to", "Transport", "")
            .o("tcp-host", "", "optional", "address the source connects to (default: destination host)", "Transport", "")
            .o("compress", "", "none", "compress the stream between hosts", "Transport", "choice:none,zstd,lz4,gzip")
            .a("send-pipe", "shell command the stream passes through on the source, repeatable (e.g. \"mbuffer -q -m 1G\")", "Transport")
            .a("receive-pipe", "shell command the stream passes through on the destination, repeatable", "Transport")
            .o("bwlimit", "", "optional", "limit the stream to this many bytes/second (e.g. 50M)", "Transport", "")
            .o("progress", "", "flag", "report progress on stderr", "Transport", "")
    }
}

/// Positional arguments of each command, in order, by option name.
fn positionals(cmd: &str) -> &'static [&'static str] {
    match cmd {
        "list" => &["glob"],
        "status" | "push" | "prune" => &["glob", "destination", "destination-host"],
        "pull" => &["glob", "source-host", "destination", "destination-host"],
        "snapshot" => &["glob", "tag", "date-format"],
        "rotate" => &["glob", "tag", "keep", "destination", "destination-host"],
        "destroy" => &["source", "source-host"],
        "rename" => &["source", "name", "source-host"],
        "receive" => &["dataset"],
        _ => &[],
    }
}

fn takes_preset(cmd: &str) -> bool {
    positionals(cmd).first() == Some(&"glob")
}

#[rustfmt::skip]
fn build(reg: &mut Registry) -> Cli {
    let mut cli = Cli::new();
    cli.name("zsync");
    cli.description("Replicate ZFS datasets selected by glob: find the newest snapshot source and destination share and send exactly what is missing.");
    cli.epilog("Run 'zsync <command> --help' for a command's options. Presets: ~/.zsync.json, /etc/zsync.json, ~/.config/zsync/zsync.conf, /etc/zsync.conf.");
    cli.role("progress", "out", "Transfer progress, one JSON object per line", "application/x-ndjson");

    {
        let c = cli.command("list", "list file systems");
        c.arg_variadic("ARGS", "[glob/preset]", "");
        c.effects(&["read-only"]);
        Reg { cli: &mut *c, reg, cmd: "list" }.ssh().select("recursive fs lookup").common(true);
        completers(c, "list", reg);
    }
    {
        let c = cli.command("status", "get the sync status between a source dataset and destination dataset");
        c.arg_variadic("ARGS", "[glob/preset] [destination] [destination-host]", "");
        c.effects(&["read-only"]);
        Reg { cli: &mut *c, reg, cmd: "status" }.ssh().select("Send all fileystems/volumes in source-dataset").dest()
            .o("continue", "c", "flag", "continue on to the next dataset if errors are encountered", "Control", "")
            .o("all", "", "flag", "run for every preset in the config files, one table with a preset column", "Selection", "")
            .common(true);
        completers(c, "status", reg);
    }
    for (name, desc, args) in [
        ("push", "push a local dataset to another dataset optionally on a remote host", "[glob/preset] [destination] [destination-host]"),
        ("pull", "pull dataset from a remote host to another dataset optionally on a remote host", "[glob/preset] [source-host] [destination] [destination-host]"),
    ] {
        let c = cli.command(name, desc);
        c.arg_variadic("ARGS", args, "");
        c.effects(&["network"]);
        Reg { cli: &mut *c, reg, cmd: name }.ssh().select("Send all fileystems/volumes in source-dataset").dest().transfer()
            .o("all", "", "flag", "run for every preset in the config files, one after another", "Selection", "")
            .common(true);
        completers(c, name, reg);
    }
    {
        let c = cli.command("snapshot", "create snapshots on datasets matching a glob using an optional tag");
        c.arg_variadic("ARGS", "[glob/preset] [tag] [dateformat]", "");
        Reg { cli: &mut *c, reg, cmd: "snapshot" }.ssh().select("Send all fileystems/volumes in source-dataset")
            .o("snapshot", "p", "optional", "exact snapshot name to use", "Snapshot", "")
            .o("tag", "", "optional", "tag name for snapshot", "Snapshot", "")
            .o("date-format", "T", "yyyymmddHHMMssl", "date format - see https://www.npmjs.com/package/dateformat", "Snapshot", "")
            .o("prefix", "", "zsync", "snapshot name prefix", "Snapshot", "")
            .o("atomic", "a", "flag", "create all possible snapshots atomically", "Snapshot", "")
            .o("continue", "c", "flag", "continue on to the next dataset if errors are encountered", "Snapshot", "")
            .o("dry-run", "", "flag", "show the snapshots without creating them", "Snapshot", "")
            .common(true);
        completers(c, "snapshot", reg);
    }
    {
        let c = cli.command("rotate", "rotate snapshots keeping a certain number");
        c.arg_variadic("ARGS", "[glob/preset] [tag] [keep] [destination] [destination-host]", "");
        c.effects(&["destructive"]);
        Reg { cli: &mut *c, reg, cmd: "rotate" }.ssh().select("Send all fileystems/volumes in source-dataset").dest()
            .o("keep", "K", "optional", "number of snapshots to keep", "Rotate", "int:0-")
            .o("snapshot", "p", "optional", "exact snapshot name to remove", "Rotate", "")
            .o("tag", "", "optional", "tag name to process for rotation", "Rotate", "")
            .o("prefix", "", "zsync", "snapshot name prefix", "Rotate", "")
            .o("continue", "c", "flag", "continue on to the next dataset if errors are encountered", "Rotate", "")
            .o("preserve-incremental", "i", "flag", "prevent destroying snapshots that are needed for an incremental push (on when a destination is given)", "Rotate", "")
            .o("destination-side", "", "flag", "rotate the destination's snapshots instead of the source's", "Rotate", "")
            .o("dry-run", "", "flag", "show what would be destroyed", "Rotate", "")
            .common(true);
        completers(c, "rotate", reg);
    }
    {
        let c = cli.command("prune", "destroy destination snapshots that no longer exist on the source");
        c.arg_variadic("ARGS", "[glob/preset] [destination] [destination-host]", "");
        c.effects(&["destructive"]);
        Reg { cli: &mut *c, reg, cmd: "prune" }.ssh().select("Send all fileystems/volumes in source-dataset").dest()
            .o("tag", "", "optional", "only snapshots with this tag", "Prune", "")
            .o("prefix", "", "zsync", "snapshot name prefix", "Prune", "")
            .o("all", "", "flag", "consider every snapshot, not only zsync-named ones", "Prune", "")
            .o("continue", "c", "flag", "continue on to the next dataset if errors are encountered", "Prune", "")
            .o("dry-run", "", "flag", "show what would be destroyed", "Prune", "")
            .common(true);
        completers(c, "prune", reg);
    }
    {
        let c = cli.command("destroy", "destroy dataset identified by source optionally on remote source-host");
        c.arg_variadic("ARGS", "[source] [source-host]", "");
        c.effects(&["destructive"]);
        Reg { cli: &mut *c, reg, cmd: "destroy" }.ssh()
            .o("recursive", "R", "flag", "recursively destroy children and snapshots", "Selection", "")
            .o("source", "s", "optional", "source-dataset, eg: pool/vol1, pool", "Selection", "")
            .o("source-host", "S", "optional", "host on which the source dataset resides", "Selection", "")
            .o("dry-run", "", "flag", "show what would be destroyed", "Control", "")
            .common(false);
        completers(c, "destroy", reg);
    }
    {
        let c = cli.command("rename", "rename dataset identified by source to name, optionally on remote source-host");
        c.arg_variadic("ARGS", "[source] [name] [source-host]", "");
        c.effects(&["destructive"]);
        Reg { cli: &mut *c, reg, cmd: "rename" }.ssh()
            .o("source", "s", "optional", "source-dataset, eg: pool/vol1, pool", "Selection", "")
            .o("source-host", "S", "optional", "host on which the source dataset resides", "Selection", "")
            .o("name", "n", "optional", "the new name of source-dataset, eg: pool/vol1-copy, pool2", "Selection", "")
            .o("recursive", "", "flag", "recursively rename snapshots (zfs rename -r)", "Selection", "")
            .o("force", "", "flag", "force unmount (zfs rename -f)", "Selection", "")
            .o("parents", "", "flag", "create missing parents (zfs rename -p)", "Selection", "")
            .o("dry-run", "", "flag", "show the rename without doing it", "Control", "")
            .common(false);
        completers(c, "rename", reg);
    }
    {
        let c = cli.command("receive", "receive a dataset via stdin");
        c.arg_variadic("ARGS", "[dataset]", "");
        c.stdin("ZFS send stream", "application/octet-stream");
        Reg { cli: &mut *c, reg, cmd: "receive" }.ssh()
            .o("force", "F", "flag", "force receive (may cause rollback)", "Receive", "")
            .o("destination", "d", "optional", "destination dataset", "Receive", "")
            .o("resumable", "", "flag", "keep partial state if interrupted (zfs receive -s)", "Receive", "")
            .o("unmounted", "", "flag", "do not mount (zfs receive -u)", "Receive", "")
            .o("abort", "", "flag", "discard partially received state (zfs receive -A) and exit", "Receive", "")
            .o("listen", "", "optional", "read the stream from one TCP connection on ADDR:PORT instead of stdin", "Receive", "")
            .common(false);
        completers(c, "receive", reg);
    }
    {
        let c = cli.command("send", "send a dataset via stdout");
        c.stdout("ZFS send stream", "application/octet-stream");
        Reg { cli: &mut *c, reg, cmd: "send" }.ssh()
            .o("incremental", "i", "optional", "do an incremental send starting with [dataset]", "Send", "")
            .o("snapshot", "s", "optional", "the final snapshot to send", "Send", "")
            .o("compressed", "m", "flag", "send blocks compressed as they are on disk", "Send", "")
            .o("raw", "W", "flag", "for encrypted datasets, send data exactly as it exists on disk", "Send", "")
            .o("intermediary", "I", "flag", "include intermediate snapshots when doing an incremental send", "Send", "")
            .o("properties", "p", "flag", "include properties in the send stream", "Send", "")
            .o("replication", "R", "flag", "create a replication send stream", "Send", "")
            .o("large-block", "", "flag", "allow blocks larger than 128K", "Send", "")
            .o("resume-token", "", "optional", "resume an interrupted receive (zfs send -t)", "Send", "")
            .o("progress", "", "flag", "parsable progress on stderr (zfs send -v -P)", "Send", "")
            .o("connect", "", "optional", "write the stream to HOST:PORT instead of stdout", "Send", "")
            .common(false);
        completers(c, "send", reg);
    }
    {
        let c = cli.command("rpc", "run one JSON request locally and print a JSON envelope (used by --remote-zsync)");
        c.arg("REQUEST", "request JSON", "", "json");
        Reg { cli: c, reg, cmd: "rpc" }.o("zfs-path", "", "zfs", "zfs command", "Options", "");
    }
    {
        let net = cli.command("net", "TCP stream helpers used by --tcp-tool zsync");
        net.command("listen", "copy one TCP connection on ADDR:PORT to stdout").arg("ADDR", "address:port to listen on", "", "");
        net.command("connect", "copy stdin to HOST:PORT").arg("ADDR", "host:port to connect to", "", "");
    }
    cli.command("version", "print the zsync version");
    cli
}

/// Resolved command-line values plus positionals and preset arrays.
struct Args {
    values: Values,
    sources: HashMap<String, &'static str>,
    positional: HashMap<String, String>,
    preset_arrays: Settings,
    reg: Registry,
}

impl Args {
    fn var(&self, long: &str) -> &str {
        self.reg.vars.get(long).map(|(v, _)| v.as_str()).unwrap_or("")
    }
    fn given_on_cli(&self, long: &str) -> bool {
        self.sources.get(long) == Some(&"cli")
    }
    /// Option (from the command line) > positional > preset/env/default.
    fn str(&self, long: &str) -> String {
        if !self.given_on_cli(long) {
            if let Some(p) = self.positional.get(long) {
                return p.clone();
            }
        }
        match self.values.get(self.var(long)) {
            clyops::Value::Str(s) => s.clone(),
            clyops::Value::Int(i) => i.to_string(),
            clyops::Value::Float(f) => f.to_string(),
            clyops::Value::Bool(b) => b.to_string(),
            _ => String::new(),
        }
    }
    fn opt(&self, long: &str) -> Option<String> {
        Some(self.str(long)).filter(|s| !s.is_empty())
    }
    fn flag(&self, long: &str) -> bool {
        match self.values.get(self.var(long)) {
            clyops::Value::Bool(b) => *b,
            _ => false,
        }
    }
    fn num(&self, long: &str) -> Result<Option<u64>, String> {
        match self.opt(long) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| format!("--{long} must be a non-negative integer, got '{v}'")),
        }
    }
    fn list(&self, long: &str) -> Vec<String> {
        let v: Vec<String> = self.values.strs(self.var(long)).into_iter().map(String::from).collect();
        if v.is_empty() {
            self.preset_arrays.get(long).cloned().unwrap_or_default()
        } else {
            v
        }
    }
}

fn die(code: i32, json: bool, msg: &str) -> ! {
    if json {
        println!("{}", json_err(msg));
    } else {
        eprintln!("zsync: {msg}");
    }
    std::process::exit(code)
}

fn load_config(explicit: Option<&str>) -> Result<Config, (i32, String)> {
    match explicit {
        Some(path) => {
            let p = PathBuf::from(path);
            if !p.is_file() {
                return Err((EXIT_CONFIG, "config file not found".into()));
            }
            Config::load_file(&p).map_err(|e| (EXIT_CONFIG, format!("{e:#}")))
        }
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let etc = std::env::var_os("ZSYNC_SYSCONFDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/etc"));
            Config::load_defaults(home.as_deref(), &etc).map_err(|e| (EXIT_CONFIG, format!("{e:#}")))
        }
    }
}

/// The first pass over a command line: the command, its positionals, and the environment that
/// carries config defaults and preset settings into the second parse.
struct Prepared {
    cmd: String,
    args: Vec<String>,
    preset_found: bool,
    preset_arrays: Settings,
    env: HashMap<String, String>,
    json: bool,
    config: Config,
}

enum PrepError {
    /// clyops can't parse the line (or it asks for help/schema/completion): let `run_with` answer.
    Usage,
    Config(i32, String, bool),
}

fn prepare(argv: &[String]) -> Result<Prepared, PrepError> {
    // clyops removes CLYOPS_FDS from the environment on its first parse; keep it for the second.
    let mut env: HashMap<String, String> = std::env::vars().collect();
    let mut reg = Registry::default();
    let mut first = build(&mut reg);
    if !matches!(first.parse(argv), Parsed::Ok) {
        return Err(PrepError::Usage);
    }
    let cmd = first.command_path().join(" ");
    let args: Vec<String> = first.values().strs("ARGS").into_iter().map(String::from).collect();
    let json = first.values().str("ZSYNC_FORMAT") == "json";

    let mut preset_arrays = Settings::new();
    let mut preset_found = false;
    let mut config = Config::default();
    if !matches!(cmd.as_str(), "rpc" | "net listen" | "net connect" | "version") {
        let explicit = Some(first.values().str("ZSYNC_CONFIG")).filter(|s| !s.is_empty());
        config = load_config(explicit).map_err(|(code, msg)| PrepError::Config(code, msg, json))?;
        let preset = args.first().filter(|_| takes_preset(&cmd)).cloned();
        let (settings, found) = config.resolve(preset.as_deref());
        if found {
            zsync::debug!("using preset {}", preset.as_deref().unwrap_or(""));
            preset_found = true;
        }
        for (long, values) in settings {
            match reg.vars.get(&long) {
                Some((_, true)) => {
                    preset_arrays.insert(long, values);
                }
                Some((var, false)) => {
                    if let Some(v) = values.last() {
                        env.insert(var.clone(), v.clone());
                    }
                }
                None => zsync::debug!("ignoring unknown config key '{long}'"),
            }
        }
    }
    Ok(Prepared { cmd, args, preset_found, preset_arrays, env, json, config })
}

/// Map positionals to their option names. A preset name fills the first (glob) slot, so the
/// words after it are the destination and so on, as in node-zsync.
fn make_args(cli: &Cli, values: Values, reg: Registry, p: &Prepared, strict: bool) -> Result<Args, String> {
    let names = positionals(&p.cmd);
    if strict && p.args.len() > names.len() && !names.is_empty() {
        return Err(format!("too many arguments for {}: {}", p.cmd, p.args[names.len()..].join(" ")));
    }
    let skip = usize::from(p.preset_found);
    let positional: HashMap<String, String> = names.iter().zip(p.args.iter()).skip(skip).map(|(n, v)| (n.to_string(), v.clone())).collect();
    let sources = reg.vars.keys().map(|l| (l.clone(), cli.source(l))).collect();
    Ok(Args { values, sources, positional, preset_arrays: p.preset_arrays.clone(), reg })
}

fn parse_args(argv: &[String]) -> (Cli, Args, String) {
    let p = match prepare(argv) {
        Ok(p) => p,
        Err(PrepError::Usage) => {
            // Let clyops handle --help, schema, completion and usage errors in the normal way.
            build(&mut Registry::default()).run_with(argv);
            std::process::exit(EXIT_ERROR);
        }
        Err(PrepError::Config(code, msg, json)) => die(code, json, &msg),
    };
    let mut reg = Registry::default();
    let mut cli = build(&mut reg);
    cli.env(p.env.clone());
    let values = cli.run_with(argv);
    let args = make_args(&cli, values, reg, &p, true).unwrap_or_else(|m| die(EXIT_ERROR, p.json, &m));
    (cli, args, p.cmd)
}

// ---------------------------------------------------------------------------------------------
// Tab completion (clyops `complete`, spec 9.2): values come from zfs (locally or over ssh),
// presets and config files, and ssh's own host lists.
// ---------------------------------------------------------------------------------------------

/// What a completer is registered on.
#[derive(Clone, Copy)]
enum Slot {
    /// The command's positionals (`ARGS`), decided by position.
    Args,
    /// An option's value.
    Opt(&'static str),
}

/// Register completers on a command for its positionals and the options that name datasets,
/// hosts, tags or snapshots.
fn completers(c: &mut Cli, cmd: &'static str, reg: &Registry) {
    let shapes = Arc::new(reg.cmds.get(cmd).cloned().unwrap_or_default());
    if !positionals(cmd).is_empty() {
        let sh = shapes.clone();
        c.complete("ARGS", move |req| complete_slot(cmd, Slot::Args, &sh, req));
    }
    for long in ["source", "source-host", "destination", "destination-host", "tag", "snapshot", "incremental"] {
        if !shapes.takes_value.contains(long) || (cmd == "snapshot" && long == "snapshot") {
            continue;
        }
        let sh = shapes.clone();
        c.complete(long, move |req| complete_slot(cmd, Slot::Opt(long), &sh, req));
    }
}

fn complete_slot(cmd: &'static str, slot: Slot, shapes: &OptionShapes, req: &CompleteRequest) -> CompleteResult {
    completion_values(cmd, slot, shapes, req).map_err(|e| e.into())
}

/// The words typed so far as an `Args` (with config and preset settings applied), and the config.
fn resolve_typed(cmd: &str, words: &[String]) -> Result<(Args, Config), String> {
    let mut argv: Vec<String> = cmd.split(' ').map(String::from).collect();
    argv.extend(words.iter().cloned());
    let (_, args, config) = parse_quiet(&argv, false).map_err(|e| e.unwrap_or_else(|| "cannot parse the words typed so far".into()))?;
    Ok((args, config))
}

/// Both parse passes without printing or exiting. `Err(None)` means clyops couldn't parse the
/// line; `Err(Some(msg))` is a config or argument error.
fn parse_quiet(argv: &[String], strict: bool) -> Result<(Cli, Args, Config), Option<String>> {
    let p = match prepare(argv) {
        Ok(p) => p,
        Err(PrepError::Usage) => return Err(None),
        Err(PrepError::Config(_, msg, _)) => return Err(Some(msg)),
    };
    let mut reg = Registry::default();
    let mut cli = build(&mut reg);
    cli.env(p.env.clone());
    match cli.parse(argv) {
        Parsed::Ok => {}
        Parsed::Error { message, .. } => return Err(Some(message)),
        Parsed::Help => return Err(None),
    }
    let values = cli.values().clone();
    let args = make_args(&cli, values, reg, &p, strict)?;
    Ok((cli, args, p.config))
}

/// Results of zfs listings made during one completion request (bash asks every completer at
/// once, and several want the same listing).
type ListMemo = HashMap<String, Result<Vec<zsync::zfs::Dataset>, String>>;
static LIST_MEMO: std::sync::OnceLock<std::sync::Mutex<ListMemo>> = std::sync::OnceLock::new();

/// List on `host` with a deadline, so a slow host never hangs the shell.
fn completion_list(ctx: &Ctx, host: &zsync::host::Host, query: zsync::zfs::ListQuery) -> Result<Vec<zsync::zfs::Dataset>, String> {
    let key = format!("{}|{}|{:?}", host.label(), ctx.backend.zfs.join(" "), query);
    let memo = LIST_MEMO.get_or_init(Default::default);
    if let Some(hit) = memo.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let timeout = std::env::var("ZSYNC_COMPLETE_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(5u64);
    let (tx, rx) = std::sync::mpsc::channel();
    let (backend, h) = (ctx.backend.clone(), host.clone());
    std::thread::spawn(move || {
        let _ = tx.send(backend.list(&h, &query).map_err(|e| format!("{e:#}")));
    });
    let result = rx
        .recv_timeout(Duration::from_secs(timeout))
        .unwrap_or_else(|_| Err(format!("listing on {} took longer than {timeout}s", host.label())));
    memo.lock().unwrap().insert(key, result.clone());
    result
}

fn completion_values(cmd: &str, slot: Slot, shapes: &OptionShapes, req: &CompleteRequest) -> Result<Vec<String>, String> {
    let walk = completion::walk(&req.words, shapes);
    let name = match slot {
        Slot::Opt(long) => {
            // The shell asks every completer on each Tab; only answer for the option being typed.
            if walk.pending.as_deref().is_some_and(|p| p != long) {
                return Err("another option is being completed".into());
            }
            long
        }
        Slot::Args => {
            if walk.pending.is_some() {
                return Err("an option value is being completed".into());
            }
            *positionals(cmd).get(walk.positionals.len()).ok_or("no more arguments")?
        }
    };
    let (args, config) = resolve_typed(cmd, &walk.parseable)?;
    let mut ctx = ctx_from(&args)?;
    // Never prompt (passwords, host keys) and give up quickly on unreachable hosts.
    let mut ssh = (*ctx.ssh).clone();
    ssh.options.extend(["BatchMode=yes".to_string(), "ConnectTimeout=3".to_string()]);
    ctx.ssh = Arc::new(ssh);
    let remote_ok = args.str("complete-remote") != "false";
    let host = |name: Option<String>| -> Result<zsync::host::Host, String> {
        let h = ctx.host(name.as_deref());
        if !h.is_local() && !remote_ok {
            return Err("remote completion is off (--complete-remote=false)".into());
        }
        Ok(h)
    };
    let datasets = |h: zsync::host::Host| -> Result<Vec<String>, String> {
        let query = zsync::zfs::ListQuery { types: args.opt("type"), ..Default::default() };
        Ok(completion_list(&ctx, &h, query)?.into_iter().map(|d| d.name).collect())
    };
    let snapshots = |h: zsync::host::Host, types: &str| -> Result<Vec<zsync::zfs::Dataset>, String> {
        let query = zsync::zfs::ListQuery { types: Some(types.into()), recursive: true, sort_createtxg: true, ..Default::default() };
        let selector = zsync::select::Selector::new(&args.str("glob"), &args.str("exclude")).map_err(|e| e.to_string())?;
        Ok(completion_list(&ctx, &h, query)?.into_iter().filter(|d| selector.matches(d.dataset())).collect())
    };
    let src = || args.opt("source-host");
    match (cmd, name) {
        (_, "glob") => {
            let mut out: Vec<String> = config.presets.keys().cloned().collect();
            // A host problem shouldn't hide the presets.
            match host(src()).and_then(datasets) {
                Ok(d) => out.extend(d),
                Err(e) if out.is_empty() => return Err(e),
                Err(_) => {}
            }
            Ok(out)
        }
        ("receive", "destination") | (_, "dataset") => datasets(host(None)?),
        (_, "destination") => datasets(host(args.opt("destination-host"))?),
        (_, "source") => datasets(host(src())?),
        (_, "source-host") | (_, "destination-host") => Ok(known_hosts(&config)),
        (_, "tag") => {
            let snaps = snapshots(host(src())?, "snapshot")?;
            Ok(completion::tags_from(&args.str("prefix"), snaps.iter().map(|d| d.short())))
        }
        ("rotate", "snapshot") => {
            let mut names: Vec<String> = snapshots(host(src())?, "snapshot")?.iter().map(|d| d.short().to_string()).collect();
            names.sort();
            names.dedup();
            Ok(names)
        }
        ("send", "snapshot") => Ok(snapshots(host(None)?, "snapshot")?.into_iter().map(|d| d.name).collect()),
        ("send", "incremental") => Ok(snapshots(host(None)?, "snapshot,bookmark")?.into_iter().map(|d| d.name).collect()),
        _ => Err(format!("nothing to complete for {name}")),
    }
}

/// Hosts from ~/.ssh/config, ~/.ssh/known_hosts and every host named in config files and presets.
fn known_hosts(config: &Config) -> Vec<String> {
    let mut hosts: std::collections::BTreeSet<String> =
        std::env::var_os("HOME").map(|h| completion::ssh_hosts(std::path::Path::new(&h))).unwrap_or_default().into_iter().collect();
    let settings = std::iter::once(&config.defaults).chain(config.presets.values());
    for s in settings {
        for key in ["source-host", "destination-host"] {
            hosts.extend(s.get(key).into_iter().flatten().filter(|h| !h.is_empty()).cloned());
        }
    }
    hosts.into_iter().collect()
}

fn ctx_from(a: &Args) -> Result<Ctx, String> {
    let port = match a.opt("ssh-port") {
        Some(p) => Some(p.parse::<u16>().map_err(|_| format!("invalid --ssh-port '{p}'"))?),
        None => None,
    };
    let ssh = SshConfig {
        program: split_words(&a.opt("ssh-program").unwrap_or_else(|| "ssh".into())),
        user: a.opt("user"),
        key: a.opt("key"),
        port,
        options: a.list("ssh-opt"),
        multiplex: a.flag("ssh-multiplex"),
    };
    let zfs = split_words(&a.opt("zfs-path").unwrap_or_else(|| "zfs".into()));
    let mode = if a.flag("remote-zsync") {
        BackendMode::RemoteZsync { zsync: a.opt("remote-zsync-path").unwrap_or_else(|| "zsync".into()) }
    } else {
        BackendMode::Agentless
    };
    Ok(Ctx { ssh: Arc::new(ssh), backend: Backend { zfs, mode }, keep_going: a.flag("continue"), dry_run: a.flag("dry-run") })
}

fn selection(ctx: &Ctx, a: &Args) -> Selection {
    Selection {
        host: ctx.host(a.opt("source-host").as_deref()),
        source: a.opt("source"),
        types: a.opt("type"),
        recursive: a.flag("recursive"),
        glob: a.str("glob"),
        exclude: a.str("exclude"),
    }
}

fn destination(ctx: &Ctx, a: &Args) -> Result<Destination, String> {
    Ok(Destination {
        base: a.str("destination"),
        host: ctx.host(a.opt("destination-host").as_deref()),
        drop: a.num("destination-drop")?.map(|n| n as usize),
        keep: a.num("destination-keep")?.map(|n| n as usize),
    })
}

fn data_path(a: &Args) -> Result<DataPath, String> {
    let e = |e: anyhow::Error| e.to_string();
    let port = a.num("tcp-port")?.unwrap_or(9091);
    Ok(DataPath {
        mode: transport::Mode::parse(&a.str("transport")).map_err(e)?,
        tcp: TcpOptions {
            tool: TcpTool::parse(&a.str("tcp-tool")).map_err(e)?,
            port: u16::try_from(port).map_err(|_| format!("invalid --tcp-port {port}"))?,
            bind: a.opt("tcp-bind"),
            connect_host: a.opt("tcp-host"),
            ..Default::default()
        },
        compression: Compression::parse(&a.str("compress")).map_err(e)?,
        send_pipes: a.list("send-pipe"),
        recv_pipes: a.list("receive-pipe"),
        bwlimit: a.opt("bwlimit").map(|b| parse_size(&b)).transpose()?,
    })
}

fn progress_sink(cli: &Cli, enabled: bool) -> Option<ProgressSink> {
    let fd = cli.fd_open("progress");
    if !enabled && fd.is_none() {
        return None;
    }
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let last = std::sync::Mutex::new(std::time::Instant::now() - Duration::from_secs(3600));
    Some(Arc::new(move |e: &ProgressEvent| {
        if let Some(fd) = fd {
            let mut line = serde_json::to_string(e).unwrap_or_default();
            line.push('\n');
            // SAFETY: writing a valid buffer to a descriptor the runner gave us.
            unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
        }
        if enabled {
            let text = transport::human_progress(e);
            if tty {
                eprint!("\r{text}\x1b[K{}", if e.done { "\n" } else { "" });
            } else {
                let mut l = last.lock().unwrap();
                if e.done || l.elapsed() >= Duration::from_secs(10) {
                    *l = std::time::Instant::now();
                    eprintln!("{text}");
                }
            }
        }
    }))
}

fn finish<T: serde::Serialize>(json: bool, outcome: &Outcome<T>, text: impl FnOnce(&Outcome<T>)) -> i32 {
    if json {
        println!("{}", json_ok(outcome));
    } else {
        text(outcome);
    }
    if outcome.failures.is_empty() {
        0
    } else {
        if !json {
            eprintln!("zsync: {} dataset(s) failed", outcome.failures.len());
        }
        EXIT_PARTIAL
    }
}

fn run(cli: &Cli, a: &Args, cmd: &str) -> Result<i32, String> {
    let json = a.str("format") == "json";
    let e = |e: anyhow::Error| format!("{e:#}");
    match cmd {
        "version" => {
            println!("zsync {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "rpc" => {
            let env = serve_rpc(split_words(&a.str("zfs-path")), a.values.str("REQUEST"));
            println!("{}", serde_json::to_string(&env).unwrap_or_default());
            Ok(if env.ok { 0 } else { EXIT_ERROR })
        }
        "net listen" => zsync::ops::stream::net_listen(a.values.str("ADDR")).map(|_| 0).map_err(e),
        "net connect" => zsync::ops::stream::net_connect(a.values.str("ADDR")).map(|_| 0).map_err(e),
        "list" => {
            let ctx = ctx_from(a)?;
            let rows = ops::list_datasets(&ctx, &selection(&ctx, a)).map_err(e)?;
            if json {
                println!("{}", json_ok(&rows));
            } else {
                for r in rows {
                    println!("{}", r.name);
                }
            }
            Ok(0)
        }
        "status" | "push" | "pull" if a.flag("all") => run_all(a, cmd),
        "status" => {
            let (out, dest_host) = status_outcome(a)?;
            Ok(finish(json, &out, |o| {
                let mut rows = vec![STATUS_HEADER.map(String::from).to_vec()];
                rows.extend(status_rows(o, &dest_host));
                println!("{}", table(&rows));
            }))
        }
        "push" | "pull" => {
            let (out, dry_run) = push_outcome(cli, a)?;
            Ok(finish(json, &out, |o| {
                if dry_run {
                    let mut rows = vec![PUSH_HEADER.map(String::from).to_vec()];
                    rows.extend(push_rows(o));
                    println!("{}", table(&rows));
                } else {
                    println!("done");
                }
            }))
        }
        "snapshot" => {
            let ctx = ctx_from(a)?;
            let o = ops::snapshot::SnapOpts {
                prefix: a.str("prefix"),
                tag: a.opt("tag"),
                date_format: a.str("date-format"),
                name: a.opt("snapshot"),
                atomic: a.flag("atomic"),
            };
            let out = ops::snapshot::snapshot(&ctx, &selection(&ctx, a), &o).map_err(e)?;
            Ok(finish(json, &out, |o| {
                if ctx.dry_run {
                    o.results.iter().for_each(|n| println!("{n}"));
                } else {
                    println!("done");
                }
            }))
        }
        "rotate" => {
            let ctx = ctx_from(a)?;
            let dest = destination(&ctx, a)?;
            let has_dest = !dest.base.is_empty();
            let o = ops::rotate::RotateOpts {
                prefix: a.str("prefix"),
                tag: a.opt("tag"),
                keep: a.num("keep")?.unwrap_or(0) as usize,
                snapshot: a.opt("snapshot"),
                preserve_incremental: a.flag("preserve-incremental") || has_dest,
                destination_side: a.flag("destination-side"),
                dest: has_dest.then_some(dest),
            };
            let out = ops::rotate::rotate(&ctx, &selection(&ctx, a), &o).map_err(e)?;
            Ok(finish(json, &out, |o| print_destroyed(ctx.dry_run, o)))
        }
        "prune" => {
            let ctx = ctx_from(a)?;
            let o = ops::rotate::PruneOpts { prefix: a.str("prefix"), tag: a.opt("tag"), all: a.flag("all"), dest: destination(&ctx, a)? };
            let out = ops::rotate::prune(&ctx, &selection(&ctx, a), &o).map_err(e)?;
            Ok(finish(json, &out, |o| print_destroyed(ctx.dry_run, o)))
        }
        "destroy" => {
            let ctx = ctx_from(a)?;
            let host = ctx.host(a.opt("source-host").as_deref());
            ops::admin::destroy(&ctx, &host, &a.str("source"), a.flag("recursive")).map_err(e)?;
            if json {
                println!("{}", json_ok(&serde_json::Value::Null));
            }
            Ok(0)
        }
        "rename" => {
            let ctx = ctx_from(a)?;
            let host = ctx.host(a.opt("source-host").as_deref());
            let o = RenameOpts { recursive: a.flag("recursive"), force: a.flag("force"), parents: a.flag("parents") };
            ops::admin::rename(&ctx, &host, &a.str("source"), &a.str("name"), &o).map_err(e)?;
            if json {
                println!("{}", json_ok(&serde_json::Value::Null));
            }
            Ok(0)
        }
        "send" => {
            let spec = SendSpec {
                snapshot: a.str("snapshot"),
                from: a.opt("incremental"),
                intermediary: a.flag("intermediary"),
                replication: a.flag("replication"),
                properties: a.flag("properties"),
                compressed: a.flag("compressed"),
                raw: a.flag("raw"),
                large_block: a.flag("large-block"),
                progress: a.flag("progress"),
                dry_run: false,
                resume_token: a.opt("resume-token"),
            };
            zsync::ops::stream::send(&split_words(&a.str("zfs-path")), &spec, a.opt("connect").as_deref()).map_err(e)?;
            Ok(0)
        }
        "receive" => {
            let dataset = a.opt("destination").or_else(|| a.opt("dataset")).unwrap_or_default();
            let zfs = split_words(&a.str("zfs-path"));
            if a.flag("abort") {
                zsync::ops::stream::receive_abort(&zfs, &dataset).map_err(e)?;
                return Ok(0);
            }
            let spec = RecvSpec {
                dataset,
                force: a.flag("force"),
                resumable: a.flag("resumable"),
                unmounted: a.flag("unmounted"),
                verbose: a.flag("verbose"),
            };
            zsync::ops::stream::receive(&zfs, &spec, a.opt("listen").as_deref()).map_err(e)?;
            Ok(0)
        }
        other => Err(format!("unknown command {other}")),
    }
}

const STATUS_HEADER: [&str; 5] = ["source", "destination", "destination-host", "up-to-date", "latest-snap-shot"];
const PUSH_HEADER: [&str; 6] = ["source", "destination", "destination-host", "action", "from", "to"];

fn status_outcome(a: &Args) -> Result<(Outcome<ops::status::StatusRow>, String), String> {
    let ctx = ctx_from(a)?;
    let dest = destination(&ctx, a)?;
    if dest.base.is_empty() {
        return Err("a destination is required (positional or -d/--destination)".into());
    }
    let out = ops::status::status(&ctx, &selection(&ctx, a), &dest, true, false).map_err(|e| format!("{e:#}"))?;
    Ok((out, dest.host.label().to_string()))
}

fn status_rows(o: &Outcome<ops::status::StatusRow>, dest_host: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = o
        .results
        .iter()
        .map(|r| {
            vec![
                r.source.clone(),
                r.destination.clone(),
                r.destination_host.clone(),
                r.up_to_date.to_string(),
                r.latest.clone().unwrap_or_default(),
            ]
        })
        .collect();
    for f in &o.failures {
        rows.push(vec![f.dataset.clone(), String::new(), dest_host.into(), "error".into(), String::new()]);
    }
    rows
}

fn push_outcome(cli: &Cli, a: &Args) -> Result<(Outcome<ops::push::PushResult>, bool), String> {
    let ctx = ctx_from(a)?;
    let sel = selection(&ctx, a);
    let opts = ops::push::PushOpts {
        dest: destination(&ctx, a)?,
        force: a.flag("force"),
        replication: a.flag("replication"),
        properties: a.flag("properties"),
        intermediary: !a.flag("latest"),
        compressed: a.flag("compressed"),
        raw: a.flag("raw"),
        large_block: a.flag("large-block"),
        unmounted: a.flag("unmounted"),
        fallback_destroy: a.flag("fallback-destroy-destination"),
        fallback_rename: a.flag("fallback-rename-destination"),
        resumable: a.str("resumable") != "false",
        abort_resume: a.flag("abort-resume"),
        retries: a.num("retries")?.unwrap_or(0) as u32,
        bookmark: a.flag("bookmark"),
        prefix: a.str("prefix"),
        path: data_path(a)?,
        progress: progress_sink(cli, a.flag("progress")),
    };
    let out = ops::push::push(&ctx, &sel, &opts).map_err(|e| format!("{e:#}"))?;
    Ok((out, ctx.dry_run))
}

fn push_rows(o: &Outcome<ops::push::PushResult>) -> Vec<Vec<String>> {
    o.results
        .iter()
        .map(|r| {
            let action = match &r.fallback {
                Some(f) => format!("{} ({f})", r.action),
                None => r.action.clone(),
            };
            vec![
                r.source.clone(),
                r.destination.clone(),
                r.destination_host.clone(),
                action,
                r.from.clone().unwrap_or_default(),
                r.to.clone().unwrap_or_default(),
            ]
        })
        .collect()
}

/// One preset's part of an `--all` run.
#[derive(serde::Serialize)]
struct PresetRun<T: serde::Serialize> {
    preset: String,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    outcome: Option<Outcome<T>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `status --all` / `push --all`: run the command once per preset, as if each preset name had
/// been given in the first slot with the rest of the command line, and report them together.
/// A preset that fails doesn't stop the others; the exit code is then 3.
fn run_all(a: &Args, cmd: &str) -> Result<i32, String> {
    let json = a.str("format") == "json";
    if !a.positional.is_empty() {
        return Err("--all runs every preset in the config files; don't also give a glob, preset or other positional".into());
    }
    let config = load_config(a.opt("config").as_deref()).map_err(|(_, m)| m)?;
    if config.presets.is_empty() {
        return Err("--all: no presets are defined in the config files".into());
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let at = argv.iter().position(|w| w == cmd).ok_or("--all: cannot find the command word")?;
    let args_for = |preset: &str| -> Vec<String> {
        let mut v: Vec<String> = argv.iter().filter(|w| *w != "--all" && *w != "--all=true").cloned().collect();
        let at = if argv[..at].iter().any(|w| w == "--all" || w == "--all=true") { at - 1 } else { at };
        v.insert(at + 1, preset.to_string());
        v
    };
    let parse = |preset: &str| -> Result<(Cli, Args), String> {
        parse_quiet(&args_for(preset), true)
            .map(|(cli, args, _)| (cli, args))
            .map_err(|e| e.unwrap_or_else(|| "cannot parse the command line for this preset".into()))
    };
    let mut failed = false;
    if cmd == "status" {
        let mut runs = Vec::new();
        let mut rows = vec![["preset"].into_iter().chain(STATUS_HEADER).map(String::from).collect::<Vec<_>>()];
        for name in config.presets.keys() {
            match parse(name).and_then(|(_, args)| status_outcome(&args)) {
                Ok((out, dest_host)) => {
                    failed |= !out.failures.is_empty();
                    rows.extend(status_rows(&out, &dest_host).into_iter().map(|r| std::iter::once(name.clone()).chain(r).collect()));
                    runs.push(PresetRun { preset: name.clone(), outcome: Some(out), error: None });
                }
                Err(e) => {
                    failed = true;
                    if !json {
                        eprintln!("zsync: {name}: {e}");
                    }
                    rows.push(vec![name.clone(), String::new(), String::new(), String::new(), "error".into(), String::new()]);
                    runs.push(PresetRun { preset: name.clone(), outcome: None, error: Some(e) });
                }
            }
        }
        if json {
            println!("{}", json_ok(&serde_json::json!({ "presets": runs })));
        } else {
            println!("{}", table(&rows));
        }
    } else {
        let mut runs = Vec::new();
        let mut rows = vec![["preset"].into_iter().chain(PUSH_HEADER).map(String::from).collect::<Vec<_>>()];
        let mut dry_run = false;
        for name in config.presets.keys() {
            match parse(name).and_then(|(cli, args)| push_outcome(&cli, &args)) {
                Ok((out, dry)) => {
                    dry_run = dry;
                    failed |= !out.failures.is_empty();
                    if !json && !dry {
                        let note =
                            if out.failures.is_empty() { String::new() } else { format!(" ({} dataset(s) failed)", out.failures.len()) };
                        println!("{name}: done{note}");
                    }
                    rows.extend(push_rows(&out).into_iter().map(|r| std::iter::once(name.clone()).chain(r).collect()));
                    runs.push(PresetRun { preset: name.clone(), outcome: Some(out), error: None });
                }
                Err(e) => {
                    failed = true;
                    if !json {
                        eprintln!("zsync: {name}: {e}");
                    }
                    runs.push(PresetRun { preset: name.clone(), outcome: None, error: Some(e) });
                }
            }
        }
        if json {
            println!("{}", json_ok(&serde_json::json!({ "presets": runs })));
        } else if dry_run {
            println!("{}", table(&rows));
        }
    }
    if failed && !json {
        eprintln!("zsync: some presets or datasets failed");
    }
    Ok(if failed { EXIT_PARTIAL } else { 0 })
}

fn print_destroyed(dry_run: bool, o: &Outcome<ops::rotate::Destroyed>) {
    if dry_run {
        for d in &o.results {
            for s in &d.snapshots {
                println!("{}:{}@{s}", d.host, d.dataset);
            }
        }
    } else {
        println!("done");
    }
}

/// Printing to a closed pipe (`zsync ... | head`, or a reader that never started) makes Rust's
/// print macros panic. Exit quietly with the status a SIGPIPE'd process gets instead, like other
/// Unix tools. Other panics keep the normal report.
fn quiet_broken_pipe() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg = payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied()).unwrap_or("");
        if msg.starts_with("failed printing to std") && msg.contains("Broken pipe") {
            std::process::exit(EXIT_BROKEN_PIPE);
        }
        default(info);
    }));
}

fn main() {
    quiet_broken_pipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        let mut reg = Registry::default();
        print!("{}", build(&mut reg).usage());
        return;
    }
    let (cli, args, cmd) = parse_args(&argv);
    let json = args.str("format") == "json";
    zsync::log::set_verbose(args.flag("verbose"));
    zsync::log::set_debug(args.flag("debug") || zsync::log::debug_from_env(std::env::var("DEBUG").ok().as_deref()));

    let _lock = match args.opt("lock-file") {
        Some(path) => {
            let wait = args.num("lock-wait").unwrap_or_else(|m| die(EXIT_ERROR, json, &m)).unwrap_or(5000);
            Some(
                zsync::lock::Lock::acquire(&path, Duration::from_millis(wait)).unwrap_or_else(|e| die(EXIT_ERROR, json, &format!("{e:#}"))),
            )
        }
        None => None,
    };
    match run(&cli, &args, &cmd) {
        Ok(code) => std::process::exit(code),
        Err(msg) => die(EXIT_ERROR, json, &msg),
    }
}
