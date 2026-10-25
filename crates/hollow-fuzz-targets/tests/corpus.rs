//! Every checked-in fuzz input still runs cleanly: the minimized inputs of
//! bugs the fuzzers found (`fuzz/regressions/<target>`) and the seed corpus
//! (`fuzz/seeds/<target>`). `cargo xtask fuzz` and cargo-fuzz start from
//! the same files.

use std::path::{Path, PathBuf};

use hollow_fuzz_targets::{TARGETS, seeds};

fn fuzz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz")
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// Runs each input on a thread with the renderer's 1 MiB stack.
fn run_all(kind: &str) -> usize {
    let mut n = 0;
    for target in TARGETS {
        for file in files(&fuzz_dir().join(kind).join(target.name)) {
            let input = std::fs::read(&file).unwrap();
            let run = target.run;
            let result = std::thread::Builder::new().stack_size(1 << 20).spawn(move || run(&input)).unwrap().join();
            assert!(result.is_ok(), "{} panicked on {}", target.name, file.display());
            n += 1;
        }
    }
    n
}

#[test]
fn regressions_pass() {
    run_all("regressions");
}

#[test]
fn seeds_pass() {
    assert!(run_all("seeds") > 100, "the seed corpus is missing");
}

/// Regenerates the binary seeds (IPC streams, certificates) from the
/// current message types and test certificates:
/// `cargo test -p hollow-fuzz-targets --test corpus -- --ignored`.
#[test]
#[ignore]
fn write_seeds() {
    let put = |target: &str, name: &str, data: &[u8]| {
        let dir = fuzz_dir().join("seeds").join(target);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), data).unwrap();
    };
    for (name, data) in seeds::renderer_seeds() {
        put("ipc_renderer", name, &data);
    }
    for (name, data) in seeds::net_seeds() {
        put("ipc_net", name, &data);
    }
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hollow-net/testdata/ct");
    for site in ["torproject", "wikipedia", "microsoft"] {
        let pem = std::fs::read_to_string(testdata.join(format!("{site}.pem"))).unwrap();
        for (i, der) in pem_certs(&pem).iter().enumerate() {
            put("ct", &format!("{site}-cert{i}"), &[[0].as_slice(), der].concat());
            if let Some(list) = seeds::sct_list_of(der) {
                put("ct", &format!("{site}-scts{i}"), &[[seeds::SCT_MODE].as_slice(), list].concat());
            }
        }
    }
}

/// The DER of each certificate in a PEM file.
fn pem_certs(pem: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut b64 = String::new();
    for line in pem.lines() {
        match line.trim() {
            "-----BEGIN CERTIFICATE-----" => b64.clear(),
            "-----END CERTIFICATE-----" => out.push(base64(&b64)),
            l => b64.push_str(l),
        }
    }
    out
}

fn base64(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|&c| c != b'=') {
        let Some(v) = ALPHABET.iter().position(|&a| a == c) else { continue };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}
