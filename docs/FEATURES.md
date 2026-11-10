# Feature parity with node-zsync 3.3.6

How each node-zsync feature maps to this rewrite, and where it is tested. `e2e` is
`tests/e2e.rs` (fake zfs), `it` is `tests/zfs_it.rs` (real ZFS), `unit` is the module's own tests.

## Commands and flags

| node-zsync | rs-zsync | tests |
|---|---|---|
| `list [glob/preset]`, `-t -g -x -R -s -S` | same | e2e `list_globs_and_excludes`, unit `select` |
| `status [glob] [dest] [dest-host]`, `-n -N -d -D` | same; same table columns; JSON adds the plan, missing snapshots and resume state | e2e `status_push_incremental_roundtrip` |
| `push`, `-L -F -r -c -e -X -m -W` | same; `-p/--properties` is now on the CLI too (it was API-only) | e2e `status_push_*`, `latest_only_*`, `diverged_*`, `modified_*`; it `latest_diverged_and_force`, `encrypted_raw_send`, `zvol_*` |
| `pull [glob] [source-host] [dest] [dest-host]` | same | e2e `local_pull_and_remote_to_remote` |
| `snapshot [glob] [tag] [dateformat]`, `-p -T -a -c` | same; the tag is `--tag` (node-zsync declared `-t` for both `--type` and `--tag`) | e2e `snapshot_naming_and_atomic`, unit `dateformat` |
| `rotate [glob] [tag] [keep] [dest] [dest-host]`, `-K -i -c` | same; `-p/--snapshot` now works (it was declared but never implemented) | e2e `rotate_*`, unit `plan::rotation*` |
| `destroy [source] [source-host]`, `-R` | same | e2e `destroy_and_rename_commands` |
| `rename [source] [name] [source-host]` | same; adds `--recursive --force --parents` (were API-only) | e2e `destroy_and_rename_commands` |
| `send`, `-i -s -I -p -R -m -W` | same | e2e `send_and_receive_commands` |
| `receive [dataset]`, `-F -d` | same | e2e `send_and_receive_commands` |
| `-u/--user`, `-k/--key` | same; adds `--ssh-port --ssh-opt --ssh-program --ssh-multiplex` | e2e `ssh_options_are_passed`, unit `host` |
| `-l/--lock-file`, `-w/--lock-wait`, `$LOCK_WAIT` | same, using flock (no stale lock files) | e2e `lock_file_contention`, unit `lock` |
| `-f json` | versioned envelope `{"version":1,"ok":..,"result":..}` | e2e (most tests use JSON) |
| `-v`, `-V`, `DEBUG=zsync` | same | unit `log` |
| `-C/--config`, `~/.zsync.json`, `/etc/zsync.json` presets | same, plus native `*.conf` files | e2e `presets_json_native_and_precedence`, unit `config` |
| exit 2 on a missing `--config` | same | e2e `presets_*` |

## Behavior

| node-zsync behavior | rs-zsync |
|---|---|
| fromSnap = newest destination snapshot that also exists on the source | same, matched by GUID with a name fallback (`plan::diff`, exhaustively tested) |
| toSnap = newest source snapshot | same |
| no common snapshot and destination has snapshots: error, unless `-e` (rename to `-renamed-<ms>`) or `-X` (destroy -r), then a forced `-R` | same |
| no common snapshot and destination empty or missing: full send, `-R` forced unless `--latest` | same |
| `nameDrop(name, drop, keep)` destination mapping | same (`naming::name_drop`, table tested) |
| rotate: `zsync-<tag>-[0-9]+`, keep newest N, `--preserve-incremental` keeps the newest common snapshot and counts it toward N, on automatically when a destination is given | same; the pattern is anchored at the start of the name |
| remote work via `ssh host zsync <cmd> --format json` | `--remote-zsync` (`zsync rpc`); the default is now agentless `zfs` over ssh |
| send → receive piping through the local process | `--transport relay` (default); also `direct` and `tcp` |
| receive errors reported before send errors | same; every pipeline stage reports its exit status |

## Bugs fixed on purpose

* Destination listings ignored `--user/--key`.
* A failed source listing crashed `each()` (it dereferenced an undefined list).
* `-t` was registered for both `--type` and `--tag` on `snapshot` and `rotate`.
* `rotate -p/--snapshot` did nothing.
* Atomic snapshots ignored the ssh user and key.
* Preset values overrode options given explicitly on the command line.
* `--continue` swallowed failures and exited 0. It now prints a summary and exits 3.
* Snapshots were matched only by name.
* `zfs list` relied on implicit ordering; zsync now asks for `-s createtxg`.
* Errors could be reported after the callback because of a fixed 500 ms wait
  (`ZSYNC_RECEIVE_CB_WAIT`). zsync now waits for every process to exit, so the wait and its
  variable are gone.

## New

Resumable receives (`-s`, `zfs send -t`, `--abort-resume`, `--retries`), transports (`direct`,
`tcp` with nc, ncat, socat or zsync), `--compress`, `--send-pipe`/`--receive-pipe`,
`--bwlimit`, `--progress` (and NDJSON progress on the clyops `progress` role), `prune`,
`rotate --destination-side`, `push --bookmark` with bookmark-based incrementals,
`--dry-run`, `--unmounted`, `--large-block`, `--zfs-path` (e.g. sudo), `--prefix`,
`zsync net listen/connect`, `zsync version`, `--help-json-schema` and shell completion
(through clyops).
