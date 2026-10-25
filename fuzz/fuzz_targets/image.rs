#![no_main]

// See crates/hollow-fuzz-targets: the same target `cargo xtask fuzz` runs.
libfuzzer_sys::fuzz_target!(|data: &[u8]| (hollow_fuzz_targets::find("image").expect("target").run)(data));
