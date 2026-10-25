//! The network process's certificate parsing, as a library.
//!
//! [`ct`] takes server certificates apart (DER, embedded SCT lists) before
//! anything has vouched for them, so it is a library of its own that
//! `crates/hollow-fuzz-targets` can drive. The `hollow-net` binary
//! (src/main.rs) is everything else and uses it as `crate::ct`.

pub mod ct;
