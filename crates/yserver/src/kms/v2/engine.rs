//! `RenderEngine` stub — Stage 1b scaffolding.
//!
//! Per rendering-model-v2 spec § "RenderEngine — drawing primitives
//! into storage", this will eventually own Vulkan pipelines for core
//! drawing ops (fill, copy, put_image, RENDER composite, traps,
//! triangles, glyphs), the per-batch state, glyph atlas, gradient
//! cache, scratch images. For Stage 1b it's an empty marker — the
//! real shape lands in Stage 2 (fill/copy/put_image minimal) and
//! Stage 3 (RENDER + glyphs).

#[allow(dead_code)]
pub(crate) struct RenderEngine;

impl RenderEngine {
    pub(crate) const fn stub() -> Self {
        Self
    }
}
