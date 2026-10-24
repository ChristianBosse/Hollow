# Hollow architecture

Version 0.1 (Phase 0 draft), 2026-10-23. Property IDs (P1–P9) refer to [threat-model.md](threat-model.md).

## 1. Principles

1. **Least privilege by process.** Each process gets only the capabilities its job needs. The code that parses hostile web content has the fewest of all.
2. **The renderer is assumed compromised.** The design is judged by what an attacker *can't* do after fully controlling a renderer.
3. **Memory-safe by default.** Rust everywhere, `#![forbid(unsafe_code)]` in every crate except a few small, audited FFI/OS crates.
4. **Uniformity over options.** No setting that changes what a website can observe. Tiers, not knobs.
5. **Nothing on disk unless asked.** RAM first; encrypted vaults only when the user creates one.
6. **Swappable engines.** Privacy lives outside the engine, so Fortress and Standard plug into the same broker.

## 2. Process model

```
                         ┌────────────────────────────────────────────┐
                         │ hollow.exe — Broker + UI (user privileges) │
                         │ • window, tabs, address bar, compositor    │
                         │ • policy: isolation keys, permissions      │
                         │ • profile in RAM, vault access             │
                         │ • validates every IPC message              │
                         └──┬──────────────┬───────────────┬──────────┘
                    IPC     │              │               │
       ┌────────────────────┘              │               └──────────────────────┐
       ▼                                   ▼                                      ▼
┌─────────────────────────┐   ┌──────────────────────────┐   ┌───────────────────────────────┐
│ hollow-fortress.exe     │   │ hollow-standard.exe      │   │ hollow-net.exe                │
│ Fortress renderer       │   │ Servo renderer           │   │ the ONLY process with sockets │
│ • LPAC, NO network cap  │   │ • LPAC, NO network cap   │   │ • Arti (Tor), pluggable       │
│ • ACG, CIG, Win32k off  │   │ • JIT off, Win32k off    │   │   transports                  │
│ • one per navigation    │   │ • one per site           │   │ • rustls, HTTP/1.1, HTTP/2    │
│ • CPU-only rendering    │   │ • reduced Web APIs       │   │ • per-site circuits & caches  │
└─────────────────────────┘   └──────────────────────────┘   └───────────────────────────────┘
       ▲                                   ▲
       │ (optional, later)                 │
┌──────┴──────────────────┐   ┌────────────┴─────────────┐
│ hollow-decode.exe       │   │ hollow-viewer.exe        │
│ image / font / AV1      │   │ sandboxed PDF/doc viewer │
│ decoding sandbox        │   │ + safe conversion        │
└─────────────────────────┘   └──────────────────────────┘
```

Clear mode (Tor off) starts a **completely separate set of processes** with its own broker state, its own RAM profile and its own `hollow-net` instance that connects directly. The two families share no memory, no IPC channel and no storage.

### 2.1 Broker / UI (`hollow.exe`)

- Owns the window. It is the only process that draws to the screen: renderers hand it finished pixel buffers through shared memory, and it presents them.
- Decides the **isolation key** (first-party site) for every request and every piece of state.
- Applies window capture exclusion (Recall, screenshots) to every top-level window.
- Holds the RAM profile and unlocks vaults.
- Keeps renderer-supplied data minimal: renderers ask for resources, the broker decides whether to fetch them.

### 2.2 Network process (`hollow-net.exe`)

- The only process with the `internetClient` capability.
- Embeds **Arti** (the Rust Tor implementation): circuits per isolation key, onion service client, vanguards, Conflux and pluggable transports (obfs4, Snowflake, WebTunnel) as Arti supports them.
- Runs TLS (rustls) and HTTP. No DNS on the host: in Tor mode, names are resolved by the Tor exit.
- Keeps every network-level cache (TLS sessions, HSTS, Alt-Svc, connection pool) **keyed by isolation key**.
- Enforces HTTPS-only, Certificate Transparency and local revocation data (no OCSP).
- Strips tracking parameters, trims cross-site Referer headers, detects bounce tracking.
- Implements **request scheduling**: all of a page's subresources are requested together, in random order, with optional padding.

### 2.3 Fortress renderer (`hollow-fortress.exe`)

- Hollow's own engine: HTML + CSS, **no JavaScript, no WASM, no WebGL, no audio**.
- A fresh process for every navigation, killed when the user leaves the page.
- Receives bytes from the broker, returns a pixel buffer plus a hit-test map (links, form fields).
- See section 4 for the engine pipeline.

### 2.4 Standard renderer (`hollow-standard.exe`)

- Servo embedded, one process per site.
- SpiderMonkey with every JIT tier disabled (baseline, Ion, Warp, regexp JIT), plus WASM and SharedArrayBuffer off.
- A reduced Web API set (section 5).
- Started only when the user grants temporary Standard mode for a site. The grant ends when the tab closes.

## 3. Windows sandbox profile

| Mitigation | Fortress | Standard | Net | Broker |
|---|---|---|---|---|
| Less Privileged AppContainer (LPAC) | ✅ | ✅ | AppContainer + `internetClient` | — |
| No network capability | ✅ | ✅ | — | ✅ (broker never opens sockets) |
| ACG (no dynamic code) | ✅ | Goal: verify SpiderMonkey's interpreter-only mode allows it | ✅ | ✅ |
| CIG (signed images only) | ✅ | ✅ | ✅ | — |
| Win32k system calls disabled | ✅ | ✅ | ✅ | — |
| CET shadow stack (`/CETCOMPAT`) | ✅ | ✅ | ✅ | ✅ |
| Control Flow Guard | ✅ | ✅ | ✅ | ✅ |
| Job object: no child processes, memory cap | ✅ | ✅ | ✅ | — |
| No file system access | ✅ | ✅ | Encrypted Tor cache only | Vault only |
| Excluded from Windows Error Reporting | ✅ | ✅ | ✅ | ✅ |

**Later:** run renderers inside a lightweight Hyper-V virtual machine so an exploit must also escape a hypervisor.

### 3.1 Phase 1 findings (measured on Windows 11, `hollow selftest`)

- **LPAC blocks Winsock entirely.** In the real renderer sandbox `WSAStartup` fails with error 10107, so not even a socket can be created. With LPAC removed (self-test layer 2), the AppContainer's own network isolation still blocks external TCP (WSAEACCES), loopback TCP/UDP and DNS. Both layers are tested on every `cargo test --workspace`.
- **`LOCALAPPDATA` is required** in a child's environment, or `CreateProcessW` fails with ERROR_ENVVAR_NOT_FOUND. Children otherwise get only `SystemRoot` and `SystemDrive` (plus `TEMP`/`TMP` pointing into its private folder for the network process).
- **ACG, Win32k lockdown and CIG are confirmed active** in the renderer by reading the policies back from inside the process.
- **`hollow-net` runs with Win32k lockdown too.** Arti's dependencies import `shell32`, `oleaut32`, `pdh` and `powrprof`, which need `user32`. They are delay-loaded (crates/hollow-net/build.rs), and the only one actually called, `shell32!SHGetKnownFolderPath` from Arti's path resolver, is patched out (third_party/tor-config-path). If a delay-loaded function is ever called, the crash filter names it.
- **The broker verifies the network process's sandbox before Tor starts.** `hollow-net` first sends a `SandboxReport` read back from Windows (AppContainer, ACG, Win32k lockdown, CIG, locked memory); the broker refuses to send `StartTor` unless all of them are in force.
- **Arti must run as a release build** in practice: directory parsing in a debug build takes many minutes.

### 3.2 RAM-only operation (DEV-01, WIN-02)

- **Tor in RAM.** Patched Arti (third_party/) keeps its directory cache (SQLite + consensus blobs) and state (guards, circuit timeouts) in memory; the keystore is disabled. Arti is pointed at a path the sandbox can't write, so any missed disk write fails loudly. Every session bootstraps fresh, about 10–15 s.
- **Locked memory.** Every process uses `hollow_sandbox::LockedAlloc`: allocations are `VirtualLock`ed (best effort, within a reserved working set) and zeroed on free. The self-test reads the lock bit back from Windows. Locked pages are still written to `hiberfil.sys` on hibernation.
- **Session-only sandbox profiles.** Windows won't start an AppContainer process without a registered profile (CreateProcessW fails with ERROR_FILE_NOT_FOUND), so profiles are registered at startup and deleted at exit, together with the file-access entries on our executables. Startup repeats the cleanup in case the last run crashed.
- **During a session** Windows itself also adds, per profile:
  - `Mappings`/`Storage` registry keys;
  - `Packages\hollow.*\AC\{INetCache,…}` folders (they stay empty);
  - two machine-wide AppIso firewall rules under HKLM that contain the user's SID.

  All of these go away with the profile at exit. `cargo xtask trace-test` checks this on every run.
- **Still left on the host** by Windows itself:
  - Prefetch/Amcache/BAM-style execution records;
  - NTFS journal entries for the profile folder and ACL changes;
  - the `Packages` folder's timestamp;
  - Explorer's `MuiCache`/`UserAssist` entries when Hollow is started from Explorer;
  - anything a hibernation writes.

  Portable mode, disguise mode and eventually HollowOS address these.

## 4. Fortress engine pipeline

Everything below is CPU-only and deterministic, so every machine produces **identical pixels** (P4).

```
bytes ─► html5ever ─► DOM ─► CSS (Stylo or own) ─► layout ─► text shaping ─► raster ─► pixels
                              │                     │          │              │
                              │                     │          rustybuzz      tiny-skia
                              │                     taffy (flex/grid) + block/inline layout
                              all stylesheets fetched regardless of media queries
```

| Stage | Candidate crates | Notes |
|---|---|---|
| HTML parsing | `html5ever` | Spec-compliant, from Servo |
| CSS | `stylo` (from Servo/Firefox) or our own | Stylo gives Firefox-like behavior, which helps Tor Browser mimicry |
| Layout | `taffy` + our own block/inline layout | |
| Fonts | `skrifa` / `read-fonts` (fontations) | Memory-safe font parsing. Bundled fonts only; web fonts come later behind the Rust parser |
| Text shaping | `rustybuzz` | Rust port of HarfBuzz |
| Raster | `tiny-skia` | Software rasterizer |
| Images | `image`, `zune-jpeg`, `png` | Optional separate decode process |
| AV1 video | `rav1d` | Later |
| SVG | `usvg` / `resvg` | Static only, no scripts or SMIL |

**Determinism rules:** no platform `libm` (use the `libm` crate), no fused multiply-add unless explicit, no system fonts, no GPU, fixed DPI, window sizes snapped to standard steps (letterboxing).

**Forms** work fully without JS (GET/POST). **Rebuild mode** extracts the readable content of pages that arrive empty without JS.

### 4.1 How a page reaches the screen (Phase 2, implemented)

1. The broker checks the URL against policy and the network process fetches it over Tor (redirects go back through the policy check).
2. A **fresh** `hollow-fortress` process gets the bytes (`Load`). It parses them with `hollow-engine` and answers with the title and the images and stylesheets it wants (`NeedResources`).
3. The broker decides what to fetch. It fetches only URLs that pass the same policy, all on **the circuit of the page in the address bar** (first-party isolation), and hands back only responses of the right type (`Resources`).
4. The renderer lays out and paints the visible part of the page (`Frame`: RGBA pixels). Scrolling and resizing send `Paint`; clicks send `HitTest`, and the renderer answers with a link, which the broker re-checks before navigating.
5. The broker composes the frame with its own bars (drawn by the same engine from broker-written HTML) and presents it with softbuffer.

The window is excluded from capture. The OS-visible window title is always "Hollow". The page area snaps to 200×100 CSS px steps (letterboxing), and history lives only in RAM. A development-only cargo feature, `dev-dump`, writes composed frames to PNG and can script navigation, because the real window can't be screenshotted.

## 5. Standard mode Web API reduction

| Removed | Reason |
|---|---|
| WebRTC | IP leaks |
| WebAssembly | Attack surface |
| SharedArrayBuffer, high-resolution timers | Spectre |
| WebGL, WebGPU | Fingerprinting, attack surface |
| WebUSB, Web Bluetooth, Web Serial, Web MIDI, Gamepad | Hardware access |
| Geolocation, Battery Status, Device Motion | Location and fingerprinting |
| Notifications, Push, Service Workers | Persistence and tracking |
| Payment Request, Credential Management | Not needed |
| Clipboard read | Data theft |

Per-site grants, each once only: Canvas (with normalized output), Web Workers, media playback.

`Date` and `performance.now()` are coarsened; the time zone is always UTC; `navigator` and screen values match the Tor Browser reference profile. Keystroke timing is jittered and mouse events are smoothed before they reach the page.

## 6. Fingerprint mimicry (Fortress)

Goal: a website can't tell a Hollow Fortress user from a Tor Browser user on the "Safest" level.

| Layer | What must match | Approach |
|---|---|---|
| TLS ClientHello | Cipher suites, extensions and their order, groups (incl. X25519MLKEM768), ALPN | Custom ClientHello builder on top of rustls, or a rustls fork. Must be updated with every Tor Browser ESR release |
| HTTP/2 | SETTINGS frame, window sizes, header order, pseudo-header order, priorities | Own HTTP/2 client config |
| HTTP headers | User-Agent, Accept, Accept-Language (`en-US`), Accept-Encoding, order | Reference profile |
| Resource loading | Which subresources are fetched, and when | Fetch all stylesheets regardless of media queries; match Firefox's behavior for images and fonts |
| Window size | Letterboxing steps | Same steps as Tor Browser |

A CI job compares Hollow's captured traffic against real Tor Browser captures for a fixed page corpus and fails on differences.

## 7. Storage and memory

- **Default profile:** RAM only, gone when Hollow closes.
- **Vaults:** optional encrypted containers (Argon2id key derivation, XChaCha20-Poly1305), optionally unlocked with a YubiKey or a TPM-sealed key. A **duress password** opens a decoy vault.
- **Secret memory:** `VirtualLock` (with a raised working-set size) so it never reaches the pagefile; `zeroize` on free; the `secrecy` crate for key material.
- **Panic key:** a global shortcut that kills every Hollow process and wipes RAM state.
- **Tor cache:** consensus and descriptors kept encrypted for fast startup, inside the vault or in RAM.

## 8. Windows hardening

| Leak | Mitigation |
|---|---|
| Recall, screenshots, screen recording | `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` on all windows |
| Pagefile / hibernation file | `VirtualLock` for secrets, no large unlocked secret buffers |
| Crash dumps | `WerAddExcludedApplication`, custom exception handler, no crash reporter |
| Clipboard history / cloud clipboard | Set `ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory = 0`, `CanUploadToCloudClipboard = 0`; auto-clear after a timeout |
| Jump lists, Recent items, thumbnails | Never registered; explicit AppUserModelID with no jump list |
| Installer traces | Portable mode: runs from a folder or USB, writes nothing to the registry |
| Defender cloud sample submission | Warning before saving downloads; open them in `hollow-viewer` first |
| IME / touch-keyboard learning | Private input scopes on all text fields |
| Search indexing | Nothing written outside the vault; the vault file is marked not-indexed |
| Print spooler copies | Print-to-PDF in RAM, metadata stripped |

**Hollow Health** runs at startup and checks: Tor reachable, no leaks, Recall enabled, telemetry level, Defender cloud submission, a VPN/proxy that might conflict, accessibility tools exposing content.

**Disguise mode** changes the window title, icon and process names to something harmless. **Hollow Lock** adds a startup password, auto-lock when idle, and a wipe after N failed attempts.

## 9. Updates and supply chain

- **TUF** (The Update Framework), via the `tough` crate: threshold signatures from several maintainers, so no single key can push an update.
- Downloaded **over Tor** at random times, from an onion mirror and public mirrors. Hollow runs no servers of its own.
- **Reproducible builds** from Phase 1, so anyone can check that the binary matches the source.
- Dependencies vendored and reviewed: `cargo-vet`, `cargo-deny`, minimal dependency count.
- Continuous **fuzzing** of every parser (HTML, CSS, images, fonts, IPC, HTTP).
- **Kani** model checking on the broker's IPC validation code.

## 10. Proposed Cargo workspace

```
hollow/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ hollow-app/             # broker + UI (hollow.exe)
│  ├─ hollow-ipc/             # message schema, validation (Kani-checked)
│  ├─ hollow-sandbox/         # AppContainer/LPAC launch, mitigation policies, job objects
│  ├─ hollow-net/             # network process: Arti, TLS, HTTP, caches, scheduling
│  ├─ hollow-tls-profile/     # ClientHello / HTTP/2 mimicry profiles
│  ├─ hollow-fortress/        # Fortress engine + renderer process
│  ├─ hollow-standard/        # Servo integration (Phase 4)
│  ├─ hollow-privacy/         # URL cleaning, metadata stripping, isolation keys, petnames
│  ├─ hollow-vault/           # encrypted vaults, duress, hardware keys
│  ├─ hollow-winharden/       # Windows hardening (capture exclusion, WER, clipboard, memory)
│  ├─ hollow-viewer/          # sandboxed document viewer + safe conversion
│  └─ hollow-update/          # TUF client
├─ fuzz/                      # cargo-fuzz targets
├─ xtask/                     # build, reproducibility, leak and fingerprint tests
└─ docs/
```

## 11. Open decisions

| Decision | Options | Leaning |
|---|---|---|
| CSS engine for Fortress | Stylo vs. our own | Stylo first (Firefox-like behavior), our own later if needed |
| Browser UI toolkit | Own UI drawn with tiny-skia, Slint, or egui | Own UI on tiny-skia: no web view, same deterministic stack |
| TLS mimicry | rustls fork vs. custom ClientHello builder | Prototype both in Phase 1 and measure |
| IPC transport | Named pipes + shared memory | Named pipes (restricted DACL) + shared memory for pixels |
| Serialization | `postcard`, `bincode`, hand-written | `postcard` with a fixed schema and length limits |
