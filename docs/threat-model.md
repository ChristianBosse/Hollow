# Hollow threat model

Version 0.1 (Phase 0 draft), 2026-10-23.

This document says who Hollow defends against, which security properties it promises, and, just as important, what it does **not** protect against. Every feature in [features.md](features.md) must trace back to a property here. If a promise can't be kept, it gets removed from this document, not quietly weakened.

## 1. Who Hollow is for

People who need the strongest anonymity a browser can give:

- journalists and their sources
- activists and dissidents under authoritarian governments
- people in countries that censor the internet
- people at risk on their own device: abusive partners, controlling families, monitored work machines
- anyone who doesn't want to be tracked, at the strictest level

Some of these people can be hurt or killed if Hollow fails. That's why the honest limits in section 6 carry the same weight as the protections.

## 2. Assets (what we protect)

| Asset | Example |
|---|---|
| **A1. Real IP address and location** | The home IP that identifies the user |
| **A2. Cross-site identity** | Being recognized as the same person on site X and site Y |
| **A3. Browsing content and history** | Which pages were visited and what was read or typed |
| **A4. Uploaded files and their metadata** | GPS location in a photo, author name in a document |
| **A5. Use of Hollow itself** | The fact that someone has Hollow or Tor at all |
| **A6. Stored secrets** | Vault contents, bookmarks, passwords |

## 3. Adversaries

| ID | Adversary | Abilities | Goal |
|---|---|---|---|
| **T1** | Trackers and ad-tech | Scripts and pixels on many sites, fingerprinting, cookie syncing | Link activity across sites (A2) |
| **T2** | Websites themselves | Full control of what they serve: HTML, CSS, JS, redirects, timing | Identify the user (A1, A2) |
| **T3** | Local network / ISP / national censor | Sees and blocks all traffic leaving the device | Learn what is visited (A3), detect or block Tor (A5) |
| **T4** | Malicious Tor relays | Run exit or guard relays; read or alter unencrypted exit traffic | Read content (A3), correlate the user |
| **T5** | **Exploit-using attackers** (e.g. state agencies) | Zero-day exploits delivered through a web page | Run code in the browser and send A1 outside Tor |
| **T6** | **Forensics / device seizure** | Physical access to the powered-off or locked machine | Recover A3, A6, prove Hollow was used (A5) |
| **T7** | **Someone sharing the device** | Uses the same Windows account, looks at the screen, opens apps | Find A3, A5 |
| **T8** | **Windows itself** | Telemetry, Recall, crash reports, Defender sample uploads, indexing | Accidentally collects A3, A4 |
| **T9** | **The Hollow developers and infrastructure** | Ship updates, could be compromised or coerced | Push a malicious update |
| **T10** | Global passive adversary | Watches large parts of the internet at once | Match traffic entering and leaving Tor by timing |

## 4. Security properties (what Hollow promises)

Each property has a test that must exist before Hollow 1.0.

| ID | Property | Defends against | How it's tested |
|---|---|---|---|
| **P1** | In Tor mode, **no packet leaves the device except through Tor.** | T2, T3, T5 | Leak test in an isolated VM: the build fails if a single non-Tor packet is seen |
| **P2** | **Full control of a renderer process can't reveal the real IP.** The renderer has no network capability and no knowledge of the IP. | T5 | Sandbox test: a renderer that tries `connect()`, DNS or raw sockets is blocked |
| **P3** | **Different sites can't link you.** Every first-party site gets its own cookies, cache, storage, connections and Tor circuit. | T1, T2 | Cross-site leak test suite (supercookies, HSTS, favicons, ETags, cache timing) |
| **P4** | **All Fortress users look identical**, and as close to Tor Browser "Safest" users as possible. | T1, T2 | Fingerprint suite compares Hollow against a reference profile across many machines |
| **P5** | **Nothing touches the disk by default.** Persistent data exists only inside an encrypted vault the user created. | T6, T7, T8 | Disk-diff test: a full session leaves no new files or registry keys outside the vault |
| **P6** | **Secrets don't leak through Windows**: no screen capture, no pagefile, no crash dumps, no clipboard history. | T6, T8 | Windows hardening test suite |
| **P7** | **Updates can't be faked** by a single compromised key or server. | T9 | Update tests using the TUF threshold-signature model |
| **P8** | **Uploaded files are stripped** of identifying metadata. | T2 | Metadata test corpus (EXIF, PDF, Office, PNG text chunks) |
| **P9** | **Tor use can be hidden** from the local network. | T3 | Automatic switch to bridges when direct Tor fails |

## 5. Assumptions

- The Windows kernel and hardware are not compromised at the moment Hollow runs.
- The Tor network as a whole is not controlled by a single adversary.
- The user downloaded Hollow from an official source and checked the signature, or uses a reproducible build.
- Cryptography holds: X25519, ML-KEM, ChaCha20-Poly1305, Argon2id, Ed25519.

## 6. What Hollow does NOT protect against

These limits will be shown clearly in the app and on the website.

1. **A compromised Windows.** Malware with admin or kernel access, keyloggers or remote-access tools can see everything. No browser can stop that. Hollow Health warns about known risk signals, but it can't detect everything.
2. **The user identifying themselves.** Logging into a personal account, typing your real name or uploading an identifying photo all break anonymity.
3. **A full global passive adversary (T10).** Padding and cover traffic make timing correlation harder, not impossible. Only the future mixnet tier aims at that.
4. **Standard mode is weaker.** With JavaScript allowed, the attack surface and fingerprinting surface are much larger. The UI says so every time.
5. **Clear mode is not anonymous.** It only isolates. It never claims to hide your IP.
6. **Physical observation.** Cameras, shoulder surfing, or someone forcing the user to unlock.
7. **A small user base.** Until Hollow has many users and the Tor Browser mimicry is verified, Hollow users may stand out. This will be stated honestly until measured otherwise.
8. **Legal risk.** Features such as the duress password and disguise mode may have legal consequences in some jurisdictions. The documentation will say so.

## 7. Mode summary

| | Fortress (default) | Standard (temporary, per site) | Clear (Tor off) |
|---|---|---|---|
| Network | Tor | Tor | **Direct, not anonymous** |
| JavaScript | None | Yes, JIT off, reduced APIs | Same as the tier chosen |
| Engine | Hollow's own | Servo | Same as the tier chosen |
| P1 / P2 | ✅ | ✅ | ❌ / ✅ |
| P3 / P5 / P6 | ✅ | ✅ | ✅ |
| P4 | ✅ (goal: match Tor Browser Safest) | Reduced | ❌ |

## 8. Open questions

- Can the TLS ClientHello and HTTP/2 fingerprint of Tor Browser be matched exactly in Rust, release after release? (See architecture: TLS mimicry.)
- How close can Fortress resource loading get to Firefox's on the same page?
- Should the duress/decoy vault ship in 1.0, given the legal risk?
- Which accessibility path (UI Automation) gives screen-reader users full access with the least exposure?
