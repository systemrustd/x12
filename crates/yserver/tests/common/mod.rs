//! Shared scaffolding for `crates/yserver/tests/*.rs` integration tests.
//!
//! Each integration-test binary in this directory compiles `common`
//! independently and uses a different subset of its surface. The
//! `dead_code` allow is the standard Rust pattern for shared test
//! modules — each test crate's view of `common` is partial.

#![allow(dead_code)]


