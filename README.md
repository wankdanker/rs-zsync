# zsync

Replicate ZFS datasets selected by glob. For each dataset zsync lists the snapshots on the source
and on the destination, finds the newest snapshot both have, and sends exactly what the
destination is missing. The snapshot sets don't have to line up: the source may have rotated
old snapshots away and the destination may keep a longer history, and zsync still finds the
right incremental base.

This is a Rust rewrite of [node-zsync](https://github.com/wankdanker/node-zsync) with the same
commands and flags, plus resumable transfers, several transports, progress, bandwidth limits,
pruning and bookmarks. The CLI is built with [clyops](https://github.com/wankdanker/clyops), so
every command also has `--help-json-schema` and shell completion.

## Install

Releases provide static Linux builds for x86_64 and aarch64. They have no library dependencies
and run on any distribution with OpenZFS (Debian 12/13, Ubuntu, Proxmox, ...). Each release has:

* `zsync_<version>-1_amd64.deb` / `_arm64.deb`: installs `/usr/bin/zsync`, shell completions and
  docs. It depends on `openssh-client` and recommends `zfsutils-linux` (contrib on Debian). A
  machine that only coordinates remote hosts doesn't need ZFS itself.
* `zsync-<version>-<arch>-linux`: the bare static executable.
* `zsync-<version>-<arch>-linux.tar.gz`: the executable, docs and completions.
* `SHA256SUMS`.

```sh
sudo apt install ./zsync_*_amd64.deb
```

Or build from source (Rust 1.75+):

```sh
cargo install --git https://github.com/wankdanker/rs-zsync
scripts/package.sh x86_64-unknown-linux-musl   # static build + tarball + .deb into dist/
```

To cut a release, set `version` in `Cargo.toml` and push a matching tag (`v4.0.0`). The Release
workflow builds both architectures, installs the `.deb` in a Debian 13 container, runs it, and
publishes everything to the GitHub release (tags with a `-suffix` become pre-releases).

zsync only needs to be installed on the machine you run it from. It reaches other hosts with
OpenSSH and runs plain `zfs` there. If you'd rather run zsync on the remote hosts too, as
node-zsync did, add `--remote-zsync`.

## Quick start

```sh
$ zsync list
pool1
pool1/sub1
pool1/sub1/swap
pool1/sub2
pool2
pool2/sub3

$ zsync list "pool1/**"
pool1/sub1
pool1/sub1/swap
pool1/sub2

$ zsync list --exclude "*/*2" "pool1/*"
pool1/sub1

$ zsync snapshot "pool1/*" hourly
done

$ zsync status --exclude "*/*2" "pool1/*" pool3 someserver
source      destination  destination-host  up-to-date  latest-snap-shot
pool1/sub1  pool3/sub1   someserver        false

$ zsync push --exclude "*/*2" "pool1/*" pool3 someserver
done

$ zsync rotate "pool1/*" hourly 24 pool3 someserver
done
```

Globs work like minimatch: `*` and `?` stay within one path element, `**` crosses elements,
`{a,b}` and `[...]` work, and lists are comma separated.

## How push decides what to send

For each dataset matched on the source:

1. The destination dataset is `<destination>/<last element of the source name>`. Change the
   mapping with `-n/--destination-drop N` (drop N elements from the left) and
   `-N/--destination-keep N` (keep N from the right). `-n 0 -N 0` keeps the whole name.
2. zsync lists both sides' snapshots, oldest first, and matches them by GUID, falling back to the
   name when a GUID isn't available. A snapshot renamed on one side still counts as common, and
   a same-named snapshot with different contents does not.
3. If the destination has the newest source snapshot, the dataset is up to date.
4. If they have a snapshot in common, zsync sends an incremental from the newest common one:
   `zfs send -I` (all intermediate snapshots), or `-i` with `-L/--latest`.
5. If the destination doesn't exist or has no snapshots, zsync sends everything. That is a
   replication stream (`zfs send -R`) unless you pass `--latest`.
6. If the destination has snapshots but none in common, the receive would fail. zsync stops,
   unless you pass `-e/--fallback-rename-destination` (rename the destination to
   `<name>-renamed-<epoch-ms>`) or `-X/--fallback-destroy-destination`.

Keep backup destinations unmodified between pushes, or incrementals will need `-F` (which rolls
back local changes). On a mounted destination even reading files changes it through access
times, so receive with `--unmounted`, set `readonly=on`, or set `atime=off` on the destination.

`zsync status` shows this plan without changing anything. With `--format json` it also shows the
planned action, which snapshots are missing, and any destination snapshots newer than the common
base (receiving those needs `-F/--force`, which rolls the destination back).

## Commands

| command | positionals | what it does |
|---|---|---|
| `list` | `[glob/preset]` | list datasets (`-t` type, `-s` root, `-R` recursive, `-S` host) |
| `status` | `[glob/preset] [destination] [destination-host]` | compare source and destination |
| `push` | `[glob/preset] [destination] [destination-host]` | send missing snapshots |
| `pull` | `[glob/preset] [source-host] [destination] [destination-host]` | push, with the source host given positionally |
| `snapshot` | `[glob/preset] [tag] [dateformat]` | create `zsync[-tag]-<timestamp>` snapshots (`-p` exact name, `-a` atomic) |
| `rotate` | `[glob/preset] [tag] [keep] [destination] [destination-host]` | keep the newest N tagged snapshots |
| `prune` | `[glob/preset] [destination] [destination-host]` | remove destination snapshots that are gone from the source |
| `destroy` | `[source] [source-host]` | `zfs destroy [-r]` |
| `rename` | `[source] [name] [source-host]` | `zfs rename [-r -f -p]` |
| `send` | | `zfs send` to stdout (or `--connect HOST:PORT`) |
| `receive` | `[dataset]` | `zfs receive` from stdin (or `--listen ADDR:PORT`) |

Run `zsync <command> --help` for the full option list. Options given explicitly win over
positionals, and both win over presets.

### Snapshots and rotation

`zsync snapshot pool/* hourly` creates `pool/x@zsync-hourly-20240302070409045`. The timestamp
uses [dateformat](https://www.npmjs.com/package/dateformat) masks (default `yyyymmddHHMMssl`).
Use `-T` to change it and `--prefix` to change `zsync`. `-a/--atomic` takes all the snapshots in
one `zfs snapshot` call.

`zsync rotate pool/* hourly 24` keeps the newest 24 snapshots named `zsync-hourly-<digits>` and
destroys the older ones. When you give a destination, `--preserve-incremental` turns on: the
newest snapshot the destination also has is never destroyed, and it counts toward the 24.
`-p NAME` destroys one named snapshot instead (still refusing to destroy the incremental base),
and `--destination-side` rotates the destination's snapshots instead of the source's.

`zsync prune pool/* backup host` destroys destination snapshots that no longer exist on the
source. By default it only touches `zsync-*` snapshots; `--all` lifts that and `--tag` narrows
it. If a destination has no snapshot in common with its source, prune leaves it alone.

`push --bookmark` bookmarks each snapshot it sends (`pool/x#<snapshot>`) and removes older
zsync bookmarks. If rotation later destroys every snapshot the destination still has, zsync
sends an incremental from the bookmark (`zfs send -i pool/x#snap`) instead of a full stream.

## Hosts and transports

Hosts are ssh destinations (`host` or `user@host`). An empty host means the local machine.
SSH options: `-u/--user`, `-k/--key`, `--ssh-port`, `--ssh-opt OPTION` (repeatable `-o`),
`--ssh-program`, `--ssh-multiplex` (ControlMaster, useful for long dataset lists). Use
`--zfs-path "sudo -n zfs"` when the remote user needs sudo.

`--transport` picks how the stream travels:

* `relay` (default): `ssh src zfs send` into zsync, then into `ssh dst zfs receive`. This works
  whenever the machine running zsync can reach both hosts, and zsync counts the bytes itself.
* `direct`: the source runs `zfs send | ssh dst zfs receive`, so data doesn't pass through
  this machine. The source needs ssh access to the destination (for example with
  `--ssh-opt ForwardAgent=yes`).
* `tcp`: the destination listens on `--tcp-port` (default 9091) and the source connects to
  `--tcp-host` (default: the destination's host name). `--tcp-tool` picks `nc` (OpenBSD
  netcat), `ncat`, `socat` or `zsync` (zsync's own `zsync net listen/connect`, which needs zsync
  on both hosts). Control still goes over ssh, but **the stream itself is not encrypted**, so
  use this only on networks you trust.

Each transport can also filter the stream:

* `--compress zstd|lz4|gzip` compresses on the source and decompresses on the destination.
* `--send-pipe CMD` and `--receive-pipe CMD` are shell commands the stream passes through on
  each side, e.g. `--receive-pipe "mbuffer -q -m 1G"`. Both are repeatable.
* `--bwlimit 50M` limits the stream to that many bytes per second (in-process for relay,
  `pv -L` on the source otherwise).
* `--progress` prints progress (size estimated with `zfs send -nvP`). When run under a clyops
  runner, progress is also written as NDJSON to the `progress` role.

Each stage reports its own exit status, even through nested ssh, so when something fails the
error names the stage and shows its stderr, with receive-side problems listed first.

## Shell completion

The `.deb` installs completion for bash, zsh and fish. Otherwise:

```sh
eval "$(zsync --completion bash)"                                  # ~/.bashrc
eval "$(zsync --completion zsh)"                                   # ~/.zshrc, after compinit
zsync --completion fish > ~/.config/fish/completions/zsync.fish    # fish
```

Tab offers values zsync looks up at that moment:

| Where | Offers |
|---|---|
| first positional (glob/preset), `-s/--source` | preset names, and datasets on the source host (`-S`, or the preset's) |
| destination positional, `-d/--destination` | datasets on the destination host (`-D`, or the preset's), so `push -D backup tank/a <Tab>` lists backup's datasets |
| host positionals, `-S`, `-D` | `Host` names from `~/.ssh/config`, plain `~/.ssh/known_hosts` entries, and every host named in presets and config |
| tag positional, `--tag` | tags of existing `zsync-<tag>-<timestamp>` snapshots |
| `rotate -p`, `send -s`, `send -i` | snapshots (and bookmarks for `-i`) |

Remote lookups run `zfs list` over ssh with `BatchMode=yes` and `ConnectTimeout=3`, so they
never prompt. Any lookup that takes longer than 5 seconds is abandoned
(`ZSYNC_COMPLETE_TIMEOUT`). To keep completion local, set `--complete-remote=false`, set
`ZSYNC_COMPLETE_REMOTE=false`, or put `complete-remote=false` in `zsync.conf`. When a lookup
fails or is off, the shell offers its usual completion instead.

The same values are available to tools through `zsync --complete-values NAME PREFIX LIMIT
OFFSET -- WORDS...` (JSON), which the clyops HTTP API, MCP server and web runner use.

## Resuming and retries

Push receives with `zfs receive -s` by default. Replication streams are the exception, because
they can't be resumed. If a transfer is interrupted, the destination keeps a
`receive_resume_token`. The next push sees the token, finishes the interrupted stream with
`zfs send -t`, and then continues as usual. `status` reports the pending resume.
`--abort-resume` discards the partial state instead (`zfs receive -A`), `--no-resumable` turns
this off, and `--retries N` retries failed transfers (resuming them) with exponential backoff.

## Presets and config

Presets let a name stand in for a set of options: `zsync push preset1`, `zsync rotate preset1
hourly 24`.

JSON, as in node-zsync (`~/.zsync.json`, `/etc/zsync.json`):

```json
{
  "preset1": {
    "glob": "pool1/*",
    "exclude": "*/*swap",
    "destination": "pool3",
    "destinationHost": "someserver",
    "replication": true,
    "force": true
  }
}
```

Native line format (`~/.config/zsync/zsync.conf`, `/etc/zsync.conf`). `key=value` lines set
defaults for every command, `name:key=value` lines belong to preset `name`, `#` starts a comment
and `@include other.conf` reads another file:

```
user=backup
ssh-opt=Compression=no
nightly:glob=tank/*
nightly:destination=backup/tank
nightly:destination-host=nas
nightly:compress=zstd
```

`-C/--config FILE` reads only that file (JSON when it starts with `{`). If the file is missing,
zsync exits with status 2. Every option can also be set through an environment variable named
`ZSYNC_<OPTION>`, e.g. `ZSYNC_DESTINATION_HOST`. Precedence, highest first: command-line
options, positionals, presets, environment, built-in defaults.

## Locking, output and exit codes

* `-l/--lock-file PATH` takes an flock on PATH so overlapping runs (cron) don't collide.
  `-w/--lock-wait MS` (default 5000, or `$LOCK_WAIT`) is how long to wait for it.
* `-f json` prints `{"version":1,"ok":true,"result":...}` or
  `{"version":1,"ok":false,"error":{"message":...}}`.
* `-v` prints what zsync is doing, and `-V` (or `DEBUG=zsync`) also prints every command it runs.
* `-c/--continue` keeps going after a dataset fails.
* `--dry-run` (push, snapshot, rotate, prune, destroy, rename) shows the plan without changing
  anything.
* Exit codes: `0` success, `1` error, `2` config file not found, `3` some datasets failed under
  `--continue`.

## Library

The crate is also a library. `zsync::plan` holds the comparison logic with no I/O (`diff`,
`decide`, `rotation`, `prune`), `zsync::ops` has one function per command, and
`zsync::backend::Backend` runs zfs locally, over ssh or through `zsync rpc`.

## Testing

```sh
cargo test                          # unit tests
cargo test --features fakes         # + end-to-end tests with a fake zfs and ssh
sudo -E env "PATH=$PATH" ZSYNC_IT_SSH_HOST=localhost \
  cargo test --features zfs-it --test zfs_it -- --test-threads=1   # real ZFS, file-backed pools
```

The fake `zfs` (`src/bin/fake_zfs.rs`) keeps per-host state in JSON and follows the receive
rules zsync depends on: GUIDs, incremental source checks, "modified since most recent snapshot",
`-F` rollback, resume tokens and fault injection. The `fake-ssh` shim maps host names to separate
state files, so the end-to-end suite runs remote-to-remote, direct, TCP and `--remote-zsync`
transfers on any machine. CI also runs the real-ZFS suite on file-backed pools, with an sshd on
localhost.

See [docs/FEATURES.md](docs/FEATURES.md) for the feature-by-feature mapping from node-zsync.

## License

MIT
