//! `PlatformBackend` stub — Stage 1b scaffolding.
//!
//! Per rendering-model-v2 spec § "PlatformBackend — hardware + OS
//! surface", this will eventually own the DRM device, KMS outputs,
//! page-flip dispatch, libinput context, scanout BOs, Vulkan device,
//! and cursor plane. For Stage 1b it's an empty marker — the real
//! shape lands in Stage 2.

#[allow(dead_code)]
pub(crate) struct PlatformBackend;

impl PlatformBackend {
    pub(crate) const fn stub() -> Self {
        Self
    }
}
