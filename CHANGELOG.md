# Changelog

## 4.0.0-alpha.3

### Added

- `status --all`, `push --all` and `pull --all` run the command for every preset in the config
  files, with one table (a `preset` column) or a `presets` array in JSON. A failing preset doesn't
  stop the others; the exit status is then 3.

### Fixed

- Writing to a closed pipe (`zsync status --help | missing-command`, `zsync list | head`) no
  longer panics: zsync exits quietly with status 141.

## 4.0.0-alpha.2

### Added

- Tab completion offers datasets (local, or remote over ssh), preset names, hosts from ssh config,
  known_hosts and presets, snapshot tags and snapshots, using clyops 0.13 dynamic completion.
  `--complete-remote=false` (or `ZSYNC_COMPLETE_REMOTE`, or `complete-remote=false` in config)
  keeps lookups local. Tools can list the same values with `zsync --complete-values`.

### Fixed

- After a preset name, the next positional is the destination (then the destination host), as in
  node-zsync. It was taken as the glob.
- `SHA256SUMS` in releases lists the `.deb` files under the names GitHub publishes.

## 4.0.0-alpha.1

Rust rewrite of node-zsync 3.3.6 that keeps its commands, flags, presets and push algorithm.
See [docs/FEATURES.md](docs/FEATURES.md) for the full mapping, the bugs fixed on purpose and the
new features (resumable receives, direct/tcp transports, compression and pipe filters,
bandwidth limits, progress, prune, bookmarks, dry runs, agentless remote hosts).
