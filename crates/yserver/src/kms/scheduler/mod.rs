//! Frame-ownership and scheduling primitives.
//!
//! See `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`.
//! Phase 1 lands the types with minimal behavior; recorders and the
//! hot-path `vkQueueWaitIdle` calls are unchanged.

pub mod damage;
