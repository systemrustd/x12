//! Rendering-model-v2 backend (Stage 1b skeleton, paint paths
//! stubbed; spec at
//! `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`).
//!
//! Lives alongside the v1 `KmsBackend`; both implement the
//! `Backend` trait and embed the shared `KmsCore` for protocol
//! bookkeeping. Startup picks v1 or v2 via `YSERVER_RENDER_MODEL`
//! (see `kms::dispatch::KmsBackendKind`).

mod backend;
pub(crate) mod engine;
pub(crate) mod glyph_atlas;
pub(crate) mod platform;
pub(crate) mod scene;
pub(crate) mod store;
pub(crate) mod telemetry;

pub use backend::KmsBackendV2;
