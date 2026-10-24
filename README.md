# Hollow

**Nothing to see. Nothing to take.**

Hollow is a privacy and anonymity browser written in Rust. It is built for people who need the strongest protection available: journalists, activists, people facing state-level surveillance, and people at risk on their own devices.

> **Status:** Phase 1 done, Phase 2 (browser window, Fortress engine) in progress. Hollow must not be used to protect anyone until it has passed an external security audit.

## Building and running (Windows)

Needs Rust (stable) and Visual Studio Build Tools with the C++ workload.

```bash
cargo build --release --workspace
```

```bash
target/release/hollow.exe
```

```bash
target/release/hollow.exe selftest
```

```bash
target/release/hollow.exe fetch https://check.torproject.org/
```

- `hollow.exe` with no arguments opens the browser window (`hollow.exe open <url>` opens it at a page). Ctrl+L focuses the address bar, Alt+Left/Right go back and forward, and the mouse wheel, Page Up/Down and Space scroll. The window is hidden from screenshots and Recall, so a screenshot of it comes out black.
- `selftest` starts the Fortress renderer in its real sandbox and tries to escape: network, DNS, loopback, your files, child processes. It also checks the network process's sandbox without starting Tor, and fails if anything gets through. It also runs as `cargo test --workspace`.
- `fetch` starts Tor inside the sandboxed network process, fetches the page, and shows it as a fresh, sandboxed Fortress renderer extracted it. Use the release build: Arti's directory parsing is very slow in debug builds.
- `HOLLOW_LOG=<filter>` (e.g. `info,tor_dirmgr=debug`) turns up the network process's logging.

### Workspace

| Crate | Binary | Role |
|---|---|---|
| `hollow-app` | `hollow.exe` | Broker: policy, starts and polices the children, validates everything they send |
| `hollow-net` | `hollow-net.exe` | The only process with sockets: Arti (Tor), rustls, HTTP |
| `hollow-fortress` | `hollow-fortress.exe` | Renderer: no network, LPAC, Win32k lockdown, ACG, CIG, one process per page |
| `hollow-engine` | — | Fortress engine: HTML + CSS subset → layout → pixels, CPU-only, bundled fonts, deterministic |
| `xtask` | — | Developer checks: `dist`, `repro-check`, `leak-test`, `trace-test` |
| `hollow-ipc` | — | Message types, framing, size limits, validation |
| `hollow-sandbox` | — | AppContainer/LPAC launch, mitigation policies, job objects (the only crate with `unsafe`) |

## What makes Hollow different

- **A hacked browser still can't leak your IP.** Renderer processes have no network access at all. Every byte goes through one small Rust network process that only speaks Tor.
- **No JavaScript by default.** The default mode, *Fortress*, runs Hollow's own memory-safe HTML/CSS engine. *Standard* mode (sandboxed Servo, JIT off) is a temporary, per-site opt-in.
- **The same pixels on every computer.** Fortress renders on the CPU with bundled fonts, so canvas, font and GPU fingerprinting don't work.
- **Hides among Tor Browser users.** In Fortress mode, Hollow's TLS, HTTP and resource-loading behavior aims to match Tor Browser's "Safest" level.
- **Built for Windows' leaks.** Hollow blocks Recall and screen capture, keeps secrets out of the pagefile, crash dumps and clipboard history, and leaves no traces in Windows.
- **Tor by default, always.** Tor is on at every start. Turning it off opens a separate, red "Clear" window that shares nothing with Tor mode.

## Documents

| Document | Contents |
|---|---|
| [Threat model](docs/threat-model.md) | Who we defend against, what we promise, what we don't |
| [Architecture](docs/architecture.md) | Processes, sandboxing, engines, data flow, crate layout |
| [Features & roadmap](docs/features.md) | Every planned feature, grouped and assigned to a phase |

## License

GPL-3.0-or-later. Small standalone libraries may be MIT/Apache-2.0 so other projects can adopt them. "Hollow" is intended to be a registered trademark, so forks must use a different name.

## Developer checks

`crates/xtask` holds checks that are run with `cargo xtask <command>` (alias in `.cargo/config.toml`).

| Command | What it checks | Fails when |
|---|---|---|
| `cargo xtask dist` | Builds the release binaries reproducibly into `target/dist/release` and prints their SHA-256 (also in `SHA256SUMS` there) | The build fails, or a build-machine path (checkout, cargo home, user profile, target dir) is embedded in a binary |
| `cargo xtask repro-check` | Freezes the sources into two copies at different paths (under `%TEMP%\hollow-repro-check`), builds each from scratch into `target/xtask-a` and `target/xtask-b` with the `dist` settings, and compares `hollow.exe`, `hollow-net.exe`, `hollow-fortress.exe` (TRUST-02) | Any hash differs |
| `cargo xtask leak-test` | Runs `hollow fetch https://check.torproject.org/` and polls the TCP/UDP tables with owning PIDs every ~10 ms (P1, host edition of TRUST-06) | `hollow.exe` or `hollow-fortress.exe` owns any socket; `hollow-net.exe` owns a UDP or listening socket or connects to anything but a Tor relay ORPort, an Arti fallback dir or a directory authority; the fetch fails; the page doesn't say "Congratulations" |
| `cargo xtask trace-test` | Snapshots files, the HKCU registry and the ACLs of the binaries before and after `hollow fetch https://check.torproject.org/`, and polls Hollow's AppContainer state while it runs (P5, host edition) | Anything new or changed matches Hollow's names, Arti's files or Hollow's AppContainer SIDs; a new AppContainer profile (`%LOCALAPPDATA%\Packages`, registry `Mappings`/`Storage`) is left; an ACE for a Hollow SID remains, or any ACE was added, on `hollow*.exe` or their folder; `%SystemRoot%\hollow-ram-only-never-created` exists; Hollow writes into its working directory; no Hollow profile was seen during the session; the fetch fails; no "Congratulations" |

All four pass `--locked` to cargo (`--unlocked` to allow `Cargo.lock` updates). `leak-test` and `trace-test` take `--url`, `--timeout <secs>`, `--no-build` (reuse `target/dist`) and `--bin-dir <dir>`.

### Reproducible builds

The toolchain is pinned to an exact version in `rust-toolchain.toml`. `dist` adds these flags on top of `.cargo/config.toml`, through `--config target.x86_64-pc-windows-msvc.rustflags=[...]`, which cargo appends to the config-file list, so the hardening flags (`+crt-static`, control-flow guard, `/CETCOMPAT`) stay defined only in `.cargo/config.toml`. `RUSTFLAGS` would replace that list instead, so `dist` removes it from the build's environment.

| Flag | Why it's needed (measured) |
|---|---|
| `-C link-arg=/Brepro` | Without it, two builds differ in the PE header and debug directory timestamps and the random GUID of the CodeView (PDB) record. These were the only differences between two same-machine builds. |
| `--remap-path-prefix=<CARGO_HOME>=/cargo` | Panic locations of registry crates are absolute paths (`C:\Users\<name>\.cargo\registry\src\...`, over 1,000 in `hollow-net.exe`). They're the same on one machine but different on every other one, and they put the builder's user name into the binary. |
| `--remap-path-prefix=<workspace>=/hollow` | Covers anything that still sees the absolute checkout path. Workspace crates already get relative paths from cargo. |
| `CARGO_INCREMENTAL=0` | Incremental compilation is off in release anyway; set explicitly so an environment override can't turn it on. |

`codegen-units = 1` is already in `[profile.release]`. Things that turned out not to be needed: remapping the target dir (nothing from `OUT_DIR` ends up in the binaries, and `dist` checks for that), remapping the Rust sysroot (std's paths are already `/rustc/<commit>/...`), `/PDBALTPATH` (rustc already writes only the PDB file name), and cl.exe flags for the C code in ring, SQLite and zstd/xz (it reaches the `.exe` only through `link.exe`, which `/Brepro` covers).

**Limits.** `repro-check` proves same-machine reproducibility: same toolchain, same Visual Studio Build Tools (the PE "Rich" header records the MSVC tool versions, 14.44.35207 here) and Windows SDK, same user profile, but different source paths and target directories. That's necessary but not sufficient. A second machine, user name and checkout path still have to produce the same hashes, and that cross-machine check is still to do.

### Leak test (host edition)

What P1 really asks for is a run in an isolated VM, with the build failing on any non-Tor packet. Windows Sandbox would provide that VM, but enabling it needs admin rights, so xtask only reports whether `WindowsSandbox.exe` exists and never changes the system. Instead it runs the host monitor. Before Hollow starts, xtask itself downloads the current microdesc consensus over plain HTTP from a directory authority (gabelmoo first). It reads Arti's compiled-in fallback dirs and authorities from the `tor-dircommon` source that `cargo metadata` points at.

Caveats, also printed on every run:
- The tables are polled, not traced, so a socket that lives shorter than one poll (~10 ms) isn't seen.
- DNS done through the Windows DNS Client service is attributed to svchost, not to Hollow.
- Only Hollow's own process tree is checked.
- The consensus isn't signature-checked. It can only make the allow-list too wide; it can't hide a connection.

### Trace test (host edition)

P5 says a session leaves nothing on disk. `trace-test` snapshots the host, runs one `hollow fetch`, snapshots again and diffs:

- **Files** (path, size, mtime): `%LOCALAPPDATA%`, `%APPDATA%`, `%TEMP%`, Documents, Downloads and Desktop (recursive; resolved through Explorer's `Shell Folders` key, so OneDrive redirection is followed), and only the top level of `%USERPROFILE%`, `%ProgramData%`, `%SystemRoot%` and the binaries' folder.
- **Registry:** all of `HKCU\Software`, plus HKLM's firewall policy (where AppContainer firewall rules live, readable without admin). Every entry is searched; binary values are decoded as UTF-16 at both alignments (RecentDocs, ShellBags) and ROT13 is undone (UserAssist).
- **ACLs** of `hollow*.exe` and their folder, as SDDL.

While Hollow runs, xtask polls every ~0.5 s for Hollow's `AppContainer\Mappings`/`Storage` keys, `Packages\hollow.*` folders, AppIso firewall rules and the ACEs on the binaries. That shows what exists mid-session: per sandbox, a `Mappings\<SID>` and a `Storage\hollow.*` key, a `Packages\hollow.*\AC\{INetCache,INetCookies,INetHistory,Temp}` folder tree, two machine-wide HKLM firewall rules (`hollow.netS-1-5-21-...-In-Block`/`-Out-Block`, created and removed by Windows with the profile), and a read/execute ACE for the SID on the binaries' folder and the `.exe`. It also checks the Hollow SIDs: xtask derives them from the names the same way Windows' `DeriveAppContainerSidFromAppContainerName` does (SHA-256 of the lower-cased name in UTF-16LE, first 28 bytes as seven RIDs) and compares them with the `Mappings\<SID>` key whose `Moniker` is `hollow.fortress` or `hollow.net`. Hollow runs in an empty working directory, which must still be empty afterwards. xtask refuses to start while any `hollow*.exe` is running, since two brokers would share and remove each other's profiles.

Hollow patterns are `hollow`, `torproject`, Hollow's two SIDs, Arti's cache/state file names (`dir.sqlite3`, `guards.json`, ...), and `arti` and `tor-` as tokens, not inside other words, so `partial` and `editor-` don't count. Other new or changed entries are listed for review but don't fail. Folders with five or more changes are collapsed to one line. A short, explicit noise filter (`NOISE` in `crates/xtask/src/trace/diff.rs`: Chrome, Edge, Firefox, Brave, the OneDrive client, VS Code, Discord, Claude) is applied only after the Hollow patterns, and what it dropped is counted per rule. xtask's own scratch folders (`target/xtask-trace`, `%TEMP%\hollow-repro-check`) aren't scanned, so a concurrent `repro-check` doesn't count as a trace. Hollow-pattern entries that were already there before the run (for example Explorer's `MuiCache`/`UserAssist` records of a `hollow.exe` once started from Explorer) are listed separately and don't fail, except leftover Hollow profiles, which startup cleanup should have removed.

The registry is read with `reg.exe export` and ACLs with `icacls.exe /save` (both from System32). The workspace forbids `unsafe`, so xtask can't call the Win32 APIs itself, and shelling out adds no dependency. `reg export` dumps all of `HKCU\Software` in under a second as UTF-16, while `reg query /s` takes about 95 s and prints in the console code page. `icacls /save` prints raw SIDs, never resolved names. Exports go to `target/xtask-trace` and are deleted after the run.

Limits, also printed on every run:
- Windows' own execution records (Prefetch, Amcache, ShimCache, BAM, SRUM, PCA logs) need admin rights to read and are **not** checked. They do record `hollow.exe`.
- The NTFS journal (`$UsnJrnl`, `$LogFile`) and `$MFT` can keep the names of files that were created and deleted, and registry hive slack and transaction logs can keep deleted keys. Neither is inspected. Neither are `hiberfil.sys`, the page/swap files or crash dumps.
- Only the listed roots are scanned. A file counts as changed when its size or timestamp changes, not its content. The `Packages` folder's own timestamp still shows that some profile existed.
- Other programs write while the test runs, and only their Hollow-pattern writes fail the test. A Hollow write with an unrelated-looking name inside a noise-filtered folder would be missed.
- `--bin-dir` pointing at a folder that another build writes into during the run (e.g. `target/release`) makes the binaries' folder look changed.
