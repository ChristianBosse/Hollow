# Patched third-party crates

Crates here replace their crates.io versions through `[patch.crates-io]` in the workspace `Cargo.toml`. Each one must say what was changed and why, and should be removed as soon as upstream ships a fix.

## saturating-time 0.5.0 (MIT OR Apache-2.0, from the Arti repository)

**Bug:** `find_limit()` searches for the smallest/largest representable time by applying ever-smaller steps until one fails at 1 ns. On Windows, `SystemTime` has 100 ns resolution, so once the step drops below 100 ns, `checked_sub` returns the *same* time instead of failing, and the loop never ends.

**Effect:** every consensus parse in `tor-netdoc` 0.47 calls `saturating_sub` on a `SystemTime`, so Arti never bootstraps on Windows. It hangs at "directory is fetching a consensus" with one CPU core at 100%.

**Patch:** in `src/internal.rs`, a step that returns an unchanged value is treated like a failed step (marked `HOLLOW PATCH`). Behaviour on platforms with nanosecond resolution is unchanged.

**Upstream:** to report at https://gitlab.torproject.org/tpo/core/arti/-/issues.

## tor-dirmgr 0.47.0 and arti-client 0.47.0 (MIT OR Apache-2.0, from the Arti repository)

**Why:** Hollow keeps Tor entirely in RAM so a session leaves nothing on disk (threat model P5, DEV-01). Upstream Arti always writes its directory cache (SQLite database + one file per consensus) and its state (guards, circuit timeouts) to disk, and has no option to stop it.

**Patches** (all marked `HOLLOW PATCH`):
- `tor-dirmgr/src/storage/sqlite.rs`: a `BlobStore` that is either the upstream on-disk directory or an in-memory map, plus `SqliteStore::new_in_memory()` (in-memory SQLite with `temp_store = MEMORY`, no lock file).
- `tor-dirmgr/src/config.rs`: `open_store()` always uses the in-memory store; `cache_dir` is never created.
- `arti-client/src/client.rs`: always uses `tor_persist::TestingStateMgr` (the in-memory state manager upstream uses on wasm) instead of `FsStateMgr`.
- `arti-client/Cargo.toml`: enables `tor-persist`'s `testing` feature, which exports `TestingStateMgr`.

**Trade-off:** Tor picks new entry guards every session (like Tails by default). Persistent guards come back later as an opt-in inside an encrypted vault.

**Not yet covered:** the `pt-client` feature (bridges) creates `<state_dir>/pt_state` on disk; it must be patched the same way before bridges are enabled (NET-11). The vendored crates' own unit tests no longer compile and are not run.

**Upgrading Arti:** re-vendor the new versions and reapply the `HOLLOW PATCH` hunks. Upstream issue #1186 ("allow override of StateMgr for ephemeral operation") would make the arti-client patch unnecessary.
