//! `KmsBackendKind` — v1/v2 selector wrapper.
//!
//! Routes `YSERVER_RENDER_MODEL=v1|v2` at startup to either
//! `KmsBackend` (v1, current production renderer) or
//! `KmsBackendV2` (v2, Stage 1b skeleton). Both implement the
//! `Backend` trait identically from the protocol-dispatcher's
//! perspective; the difference is what each does on paint paths.
//!
//! This enum lives between `lib.rs`'s pre-`Box<dyn Backend>`
//! lifecycle (where it needs concrete-type method access for
//! `fb_dimensions`, `randr_outputs`, `take_input_ctx`,
//! `composite_and_flip`) and the wrap into the trait-object that
//! `run_core` consumes. After `into_dyn_backend()` the two
//! variants are indistinguishable.

use std::io;

use yserver_core::backend::Backend;

use crate::kms::{backend::KmsBackend, v2::KmsBackendV2};

/// Two-variant wrapper for either the v1 `KmsBackend` or the v2
/// `KmsBackendV2`. Constructed at startup via
/// [`Self::open_from_env`] and consumed by `lib.rs` after the
/// pre-wrap setup is done.
///
/// V1 carries ~2.3 KiB of state (DRM, Vk pools, scheduler, etc.)
/// vs V2's ~760 B today. Boxing either variant would just push
/// the heap allocation around — `KmsBackendKind` lives for one
/// startup-then-convert hop, then disappears into
/// `Box<dyn Backend>`. Suppress the large-variant lint.
#[allow(clippy::large_enum_variant)]
pub enum KmsBackendKind {
    V1(KmsBackend),
    V2(KmsBackendV2),
}

impl KmsBackendKind {
    /// Open the backend selected by `YSERVER_RENDER_MODEL`.
    /// Default (unset / empty / `v1`) is the v1 path; `v2`
    /// constructs the Stage 1b skeleton.
    ///
    /// # Errors
    ///
    /// Returns the underlying open error from either backend, or
    /// `io::Error::Other` if the env var holds an unrecognised
    /// value.
    pub fn open_from_env(device_path: &str) -> io::Result<Self> {
        match std::env::var("YSERVER_RENDER_MODEL").as_deref() {
            Ok("v2") => {
                log::info!("yserver: render model = v2 (Stage 1b skeleton — paint paths log gaps)");
                Ok(Self::V2(KmsBackendV2::open(device_path)?))
            }
            Ok("v1") | Err(_) => {
                log::info!("yserver: render model = v1");
                Ok(Self::V1(KmsBackend::open(device_path)?))
            }
            Ok("") => {
                log::info!("yserver: render model = v1 (empty YSERVER_RENDER_MODEL)");
                Ok(Self::V1(KmsBackend::open(device_path)?))
            }
            Ok(other) => Err(io::Error::other(format!(
                "YSERVER_RENDER_MODEL={other:?} — expected 'v1' or 'v2'"
            ))),
        }
    }

    /// Virtual-screen extent for either variant.
    #[must_use]
    pub fn fb_dimensions(&self) -> (u16, u16) {
        match self {
            Self::V1(b) => b.fb_dimensions(),
            Self::V2(b) => b.fb_dimensions(),
        }
    }

    /// RandR output list for either variant.
    #[must_use]
    pub fn randr_outputs(&self) -> Vec<yserver_core::randr::RandrOutput> {
        match self {
            Self::V1(b) => b.randr_outputs(),
            Self::V2(b) => b.randr_outputs(),
        }
    }

    /// Hand the libinput context off to the input thread.
    #[must_use]
    pub fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        match self {
            Self::V1(b) => b.take_input_ctx(),
            Self::V2(b) => b.take_input_ctx(),
        }
    }

    /// Pre-loop composite + flip. v1 paints a real first frame;
    /// v2 logs a gap and returns `Ok`.
    ///
    /// # Errors
    ///
    /// Propagates v1's composite errors. v2 never errors here.
    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        match self {
            Self::V1(b) => b.composite_and_flip(),
            Self::V2(b) => b.composite_and_flip(),
        }
    }

    /// Box the chosen variant as a trait object. Consumes `self`.
    #[must_use]
    pub fn into_dyn_backend(self) -> Box<dyn Backend> {
        match self {
            Self::V1(b) => Box::new(b),
            Self::V2(b) => Box::new(b),
        }
    }

    /// View the variant as a `&mut dyn Backend` without consuming.
    /// Used by `lib.rs` to pass the backend into `run_core` while
    /// keeping the concrete-typed lifecycle hook (`disable_output`)
    /// available afterwards.
    pub fn as_dyn_backend_mut(&mut self) -> &mut dyn Backend {
        match self {
            Self::V1(b) => b,
            Self::V2(b) => b,
        }
    }

    /// Post-loop teardown — disable each output. v1 walks the
    /// scanout pools and disarms them before issuing the
    /// `disable_output` KMS commit; v2 has no scanout pools, so
    /// the disarm step collapses to walking outputs directly.
    ///
    /// # Errors
    ///
    /// Propagates the first per-output failure (other outputs
    /// already disabled keep going).
    pub fn disable_output(&mut self) -> io::Result<()> {
        match self {
            Self::V1(b) => b.disable_output(),
            Self::V2(b) => b.disable_output(),
        }
    }
}
