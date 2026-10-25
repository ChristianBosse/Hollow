# Hollow features & roadmap

Version 0.1 (Phase 0 draft), 2026-10-23. Every brainstormed feature is kept. Each has an ID, the threat-model property it serves (see [threat-model.md](threat-model.md)) and a target phase.

## Roadmap

| Phase | Name | Done when |
|---|---|---|
| **0** | Design | Threat model, architecture and this catalog agreed |
| **1** | Foundation | Broker, IPC, network process with Arti, sandboxed renderer that **provably** can't reach the network, VM leak test, reproducible build. **Status: done.** Broker, IPC, Arti network process, sandboxed renderer, `hollow selftest` (both processes, both sandbox layers), Win32k lockdown for `hollow-net`, same-machine reproducible builds (`cargo xtask repro-check`) and the host-level leak test (`cargo xtask leak-test`). Still open: the leak test inside a VM (needs Windows Sandbox/Hyper-V, which this machine doesn't have enabled) and cross-machine reproducibility |
| **2** | Fortress v0 | Real pages render: HTML, CSS, text, images, links, forms, tabs, per-site isolation. **Status: done** (2026-10-24). Wikipedia (English, Japanese, Arabic), DuckDuckGo's onion service and torproject.org render and work over Tor in the real window, and Hollow passes 52.4% of 12,135 script-free WPT CSS reftests (`cargo xtask wpt`; Chrome, Firefox and Safari pass about 88–89% of the same tests):
- **Engine:** HTML (scripting off) and a large CSS subset: linked stylesheets, `@media`, custom properties and `var()`, `calc()`/`min()`/`max()`/`clamp()`, attribute, sibling and structural selectors, `:is`/`:where`/`:not`. Generated content: `::before`/`::after` with strings, `attr()`, counters (`counter()`/`counters()`, `counter-reset`/`-increment`/`-set`), quotes and `url()` images. Layout covers block, inline and inline-block, CSS tables on any element with anonymous table boxes (`border-collapse`, `border-spacing`, `table-layout: fixed`, captions, cell `vertical-align`), flexbox, floats, relative/absolute/fixed positioning, `z-index` and CSS 2.1 paint order, borders with diagonal joins and rounded corners, heights and overflow clipping. Backgrounds: `url()` images and linear/radial/repeating gradients in multiple layers, with `background-repeat`/`-position`/`-size`/`-origin`/`-clip` and the shorthand. Text: bundled DejaVu fonts with a fixed fallback chain for Hebrew, Arabic, Devanagari, Thai, CJK, hangul and (monochrome) emoji (see architecture.md §4); the Unicode bidi algorithm with `dir` (`ltr`/`rtl`/`auto`), `<bdi>`/`<bdo>`, `direction` and `unicode-bidi`, right-to-left lines, start/end alignment, list markers, block margins, static positions and flex rows; CJK line breaking with light kinsoku rules and hanging ideographic spaces; `text-align: justify`/`justify-all`, `text-align-last`, `text-justify: none`, `text-indent`, `word-spacing`, logical margins, paddings and sizes, `text-transform: full-size-kana`. PNG/JPEG/GIF/WebP images and static SVG (files, CSS backgrounds and inline `<svg>` with page-wide `<use>` symbols and `currentColor`, rasterized by resvg with the bundled fonts only and no external references, MODE-03), `data:` URLs (images, stylesheets, `<object>` images) decoded in the engine. Acid2 matches its reference except the fixed-position scalp.
- **Browsing:** tabs (each with its own navigator thread and sandboxed renderer; the four most recently shown keep a live renderer, the rest re-render from RAM), links (middle or Ctrl+click for a background tab), scrolling, back/forward per tab, letterboxing, address-bar search through DuckDuckGo's onion service (STATE-07: nothing is sent while typing), automatic Onion-Location (NET-09), circuits and connection state keyed on the registrable domain from the bundled Public Suffix List (NET-05, STATE-01), `Referer` trimmed to the origin across origins and dropped from HTTPS to HTTP (STATE-05). Hidden `opacity: 0` content stays hidden.
- **Forms (MODE-02):** text, password, textarea, checkbox, radio, select (drop-down list and list box), buttons, labels, `fieldset` disabling, caret editing, Tab focus, Enter submits, reset; GET, POST urlencoded, multipart and text/plain with `formaction`/`formmethod`/`formenctype`/`target=_blank`. The renderer only reports the entries; the broker checks the target against policy and encodes the body itself. Typed text stays in the sandboxed renderer until submitted.
- **TLS:** post-quantum X25519MLKEM768 key exchange offered first, in pure Rust (NET-17); Certificate Transparency required under Chrome's policy, checked against Google's signed log list bundled in `crates/hollow-net/ct/` (NET-15; embedded SCTs only, `.onion` exempt); a separate, bounded TLS session cache per first-party site (STATE-03; HSTS, ETags and Alt-Svc have nothing to isolate yet: HTTPS-only, no HTTP cache, HTTP/1.1, no connection reuse).
- **Fuzzing (SEC-07):** a fuzz target for every parser that reads untrusted bytes (HTML, CSS, images, layout and paint, forms, the IPC messages the broker reads from its children, URL policy, form encoding, CT/SCT parsing) in `crates/hollow-fuzz-targets`, run by cargo-fuzz/libFuzzer (`fuzz/`, nightly, works on Windows and Linux) and by `cargo xtask fuzz` on the pinned stable toolchain; seed corpora and dictionaries are checked in and replayed by `cargo test`. Not yet continuous: there is no CI, and `.github/workflows/fuzz.yml` (nightly libFuzzer runs on Linux) is ready for when there is.

Known gaps, carried forward (engine work continues alongside later phases): grid layout (grid containers are laid out as blocks), `::first-line`/`::first-letter`/`::marker`, `rowspan`, backgrounds and borders on inline elements, partial `opacity`, `:has()`, CSS masks, declarative shadow DOM (MDN's menus), vertical writing modes, color emoji, dictionary line breaking (Thai, Khmer, ...), `letter-spacing`, bidi inside form fields (their words keep their own direction, the field stays left to right). `position: fixed` (and `background-attachment: fixed`) deliberately scrolls with the page: without scripts an overlay could never be dismissed |
| **3** | Windows hardening | RAM profile, capture exclusion, WER, clipboard, memory locking, Clear mode, panic key, portable mode. **Pulled forward and done:** RAM-only Tor (DEV-01), locked + zeroed memory (WIN-02), capture exclusion (WIN-01), session-only sandbox profiles with cleanup |
| **4** | Standard mode | Servo embedded with JIT off and reduced APIs, temporary per-site grants |
| **5** | Anti-fingerprinting & privacy | Tor Browser mimicry, letterboxing, metadata stripping, URL cleaning, padding, petnames |
| **6** | Trust & public beta | TUF updates, vaults, Hollow Lock, disguise mode, external audit, bug bounty |
| **7+** | Later | P2P sync, mixnet tier, Hyper-V renderers, HollowOS, Linux/Android/macOS |

## Network — NET

| ID | Feature | Serves | Phase |
|---|---|---|---|
| NET-01 | Arti (Rust Tor) built in; the network process is the only one with sockets | P1, P2 | 1 |
| NET-02 | Tor on at every start; off never persists | P1 | 1 |
| NET-03 | **Clear mode**: separate processes and profile, red border, "NOT ANONYMOUS" label | P1 | 3 |
| NET-04 | No drag-and-drop between modes, warning on cross-mode paste, .onion blocked in Clear | P1 | 3 |
| NET-05 | Separate Tor circuit per first-party site | P3 | 2 |
| NET-06 | New identity, optional automatic identity rotation on a timer | P3 | 3 |
| NET-07 | **Burner tab**: completely fresh identity, shares nothing | P3 | 3 |
| NET-08 | No host DNS; names resolved by the Tor exit | P1 | 1 |
| NET-09 | Onion-first: follow Onion-Location automatically | P1 | 2 |
| NET-10 | Vanguards and Conflux (as Arti supports them) | P1 | 3 |
| NET-11 | Pluggable transports: obfs4, Snowflake, WebTunnel | P9 | 3 |
| NET-12 | **Automatic stealth start**: switch to bridges when direct Tor fails | P9 | 3 |
| NET-13 | Encrypted Tor directory cache for fast startup | — | 3 |
| NET-14 | Zero network activity at startup except Tor bootstrap | P1 | 1 |
| NET-15 | HTTPS-only, Certificate Transparency required | T4 | 2 |
| NET-16 | No OCSP; local revocation list (CRLite-style) | P3 | 5 |
| NET-17 | Post-quantum TLS key exchange (X25519MLKEM768) | T3, T4 | 2 |
| NET-18 | Traffic padding and cover traffic, adjustable by tier | T10 | 5 |
| NET-19 | Website-fingerprinting defenses (packet size and timing shaping) | T3, T10 | 5 |
| NET-20 | All subresources requested together, in random order | P4, T10 | 5 |
| NET-21 | OnionShare-style file sharing through a temporary .onion | A4 | 6 |
| NET-22 | Optional mixnet tier (Nym/Loopix-style) | T10 | 7+ |
| NET-23 | Never implemented: WebRTC, WebUSB, Bluetooth, Serial, MIDI, geolocation, battery | P1, P4 | 1 |
| NET-24 | Clear mode DNS only through Oblivious DoH, never the system resolver | A3 | 3 |

## Isolation & exploit resistance — SEC

| ID | Feature | Serves | Phase |
|---|---|---|---|
| SEC-01 | Renderers in LPAC with **no network capability** | P2 | 1 |
| SEC-02 | Broker validates every IPC message; renderer never learns the IP | P2 | 1 |
| SEC-03 | Disposable Fortress renderer: fresh process per navigation | P2 | 2 |
| SEC-04 | ACG, CIG, Win32k lockdown, CET shadow stack, CFG, job objects | P2 | 1 |
| SEC-05 | Memory-safe parsers: html5ever, fontations, image-rs, zune-jpeg, rav1d, rustls | P2 | 2 |
| SEC-06 | `#![forbid(unsafe_code)]` outside small, audited OS/FFI crates | P2 | 1 |
| SEC-07 | Continuous fuzzing of every parser. **Status:** targets, both fuzzers and corpora done (see Phase 2); continuous runs wait for CI | P2 | 2 |
| SEC-08 | Kani model checking of IPC validation | P2 | 3 |
| SEC-09 | Sandboxed viewer for downloaded PDFs, images and documents | T5 | 6 |
| SEC-10 | Safe conversion (render to pixels, rebuild a clean PDF) | T5 | 6 |
| SEC-11 | Optional separate decode process for images, fonts and video | P2 | 5 |
| SEC-12 | Renderers inside a lightweight Hyper-V VM | P2 | 7+ |

## Fortress & Standard modes — MODE

| ID | Feature | Serves | Phase |
|---|---|---|---|
| MODE-01 | **Fortress default**: own engine, no JS/WASM/WebGL/audio | P2, P4 | 2 |
| MODE-02 | Full HTML forms without JS | — | 2 |
| MODE-03 | Static SVG only | P2 | 2 |
| MODE-04 | Bundled fonts; web fonts later, behind the Rust font parser | P2, P4 | 2 |
| MODE-05 | AV1 video via rav1d | — | 5 |
| MODE-06 | **Temporary Standard mode**: one-click JS for a site, ends when the tab closes | — | 4 |
| MODE-07 | **Rebuild mode**: extract readable content from pages that need JS | — | 4 |
| MODE-08 | Fortress compatibility badge per site | — | 4 |
| MODE-09 | Local-only breakage reports, sent over Tor only if the user chooses | — | 6 |
| MODE-10 | Standard: all JIT tiers off, no WASM, no SharedArrayBuffer | P2 | 4 |
| MODE-11 | Standard: reduced Web API set (see architecture §5) | P2, P4 | 4 |
| MODE-12 | Standard: per-API, one-time permissions (Canvas, Workers, media) | P4 | 4 |
| MODE-13 | Standard: one process per site, coarse timers | P2, P3 | 4 |

## Anti-fingerprinting — FP

| ID | Feature | Serves | Phase |
|---|---|---|---|
| FP-01 | **Identical pixels on every machine**: CPU rendering, bundled fonts, no GPU | P4 | 2 |
| FP-02 | Letterboxing with Tor Browser's size steps | P4 | 5 |
| FP-03 | Sites always see `en-US` and UTC, whatever the UI language | P4 | 2 |
| FP-04 | **Tor Browser "Safest" mimicry**: TLS, HTTP/2, headers, resource loading | P4 | 5 |
| FP-05 | Fetch all stylesheets regardless of media queries | P4 | 2 |
| FP-06 | Keystroke timing jitter, mouse movement smoothing (Standard mode) | P4 | 5 |
| FP-07 | No fingerprint-affecting settings; tiers only | P4 | 1 |
| FP-08 | No extensions at launch | P4 | 1 |
| FP-09 | One tracker blocklist, identical for every user, third-party requests only | P3, P4 | 5 |
| FP-10 | Fingerprint test suite and traffic comparison against real Tor Browser in CI | P4 | 5 |

## State & cross-site tracking — STATE

| ID | Feature | Serves | Phase |
|---|---|---|---|
| STATE-01 | First-party isolation of all state | P3 | 2 |
| STATE-02 | No persistent HTTP cache in Fortress | P3, P5 | 2 |
| STATE-03 | Per-site HSTS, favicons, ETags, Alt-Svc, TLS sessions; no cross-site connection reuse | P3 | 2 |
| STATE-04 | Tracking parameter removal (`utm_`, `fbclid`, `gclid`, …) | P3 | 5 |
| STATE-05 | Cross-site Referer trimming | P3 | 2 |
| STATE-06 | Bounce-tracking detection | P3 | 5 |
| STATE-07 | No search suggestions | A3 | 2 |
| STATE-08 | Clipboard: no site reads; pasted text cleaned of hidden characters | A3, A4 | 3 |
| STATE-09 | **Metadata stripping** on every upload (EXIF, GPS, PDF, Office) | P8 | 5 |
| STATE-10 | Optional image re-encoding on upload | P8 | 5 |

## Device & forensics — DEV

| ID | Feature | Serves | Phase |
|---|---|---|---|
| DEV-01 | RAM-only profile by default | P5 | 3 |
| DEV-02 | Encrypted vaults (Argon2id + XChaCha20-Poly1305) | P5 | 6 |
| DEV-03 | Duress password opens a decoy vault | T6 | 6 |
| DEV-04 | Vault unlock with YubiKey or TPM-sealed key | T6 | 6 |
| DEV-05 | **Panic key**: kill everything, wipe RAM | T6, T7 | 3 |
| DEV-06 | **Disguise mode**: harmless name, icon and process names | P5, T7 | 6 |
| DEV-07 | **Hollow Lock**: startup password, idle lock, wipe after N failures | T7 | 6 |
| DEV-08 | Portable mode: no installer, no registry, runs from USB | P5 | 3 |
| DEV-09 | No Start menu entry, file associations or default-browser registration | P5 | 3 |
| DEV-10 | Print-to-PDF in RAM with metadata stripped | P5, P8 | 6 |

## Windows hardening — WIN

| ID | Feature | Serves | Phase |
|---|---|---|---|
| WIN-01 | Capture exclusion on every window (Recall, screenshots, recording) | P6 | 3 |
| WIN-02 | `VirtualLock` + `zeroize` for secrets; pagefile-safe | P6 | 3 |
| WIN-03 | Excluded from Windows Error Reporting; no crash reporter | P6 | 1 |
| WIN-04 | Clipboard history / cloud clipboard exclusion, auto-clear | P6 | 3 |
| WIN-05 | No jump lists, Recent items or thumbnails | P5 | 3 |
| WIN-06 | Defender cloud-submission warning for downloads | P6 | 6 |
| WIN-07 | Private input scopes so the IME doesn't learn typed text | P6 | 3 |
| WIN-08 | Nothing for Windows Search to index | P5 | 3 |
| WIN-09 | **Hollow Health** startup check (Tor, leaks, Recall, telemetry, Defender, VPN) | P1, P6 | 3 |
| WIN-10 | Accessibility as an explicit choice with a clear warning, always available | — | 6 |

## Interface & phishing — UX

| ID | Feature | Serves | Phase |
|---|---|---|---|
| UX-01 | **Network ledger**: live view of every request, where it went and why | P1 | 3 |
| UX-02 | Always-visible tier and mode indicator; honest about what's protected | — | 2 |
| UX-03 | **Petnames** for .onion and other sites | T2 | 5 |
| UX-04 | Lookalike domains always shown as raw punycode | T2 | 2 |
| UX-05 | First-visit warning | T2 | 5 |
| UX-06 | Hollow brand: hollow-ring logo, dark minimal UI, red in Clear mode | — | 2 |

## Trust & project — TRUST

| ID | Feature | Serves | Phase |
|---|---|---|---|
| TRUST-01 | GPL-3.0-or-later; registered "Hollow" trademark | T9 | 0 |
| TRUST-02 | Reproducible builds | P7 | 1 |
| TRUST-03 | `cargo-vet`, `cargo-deny`, vendored, minimal dependencies | P7 | 1 |
| TRUST-04 | TUF updates with threshold signatures, fetched over Tor at random times | P7 | 6 |
| TRUST-05 | No Hollow servers, no telemetry, no accounts | T9 | 1 |
| TRUST-06 | Automated VM leak test on every build | P1 | 1 |
| TRUST-07 | Public threat model (this repo) | — | 0 |
| TRUST-08 | External security audit before 1.0 | — | 6 |
| TRUST-09 | Bug bounty, public security reports | — | 6 |
| TRUST-10 | Warrant canary | T9 | 6 |
| TRUST-11 | Funding from grants (NLnet, OTF) and donations; no venture capital | T9 | 0 |

## Later — LATER

| ID | Feature | Phase |
|---|---|---|
| LATER-01 | Peer-to-peer sync over .onion with QR pairing, no server | 7+ |
| LATER-02 | HollowOS: a Tails-like live system built around Hollow | 7+ |
| LATER-03 | Linux, Android, macOS | 7+ |
| LATER-04 | Gradually replace Servo parts with Hollow's own engine | 7+ |
