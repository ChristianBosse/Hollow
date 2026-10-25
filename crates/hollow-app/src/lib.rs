//! The broker's pure decision logic, as a library: which URLs Hollow will
//! load ([`policy`]) and how a form submission from a renderer becomes a
//! request ([`form`]).
//!
//! Both take untrusted input (typed addresses, page links, whatever a
//! compromised renderer sends) and touch nothing but their arguments, so they
//! are a library of their own that `crates/hollow-fuzz-targets` can drive.
//! The `hollow` binary (src/main.rs) is everything else and uses them as
//! `crate::policy` and `crate::form`.

pub mod form;
pub mod policy;
