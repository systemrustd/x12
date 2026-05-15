//! `SceneCompositor` stub — Stage 1b scaffolding.
//!
//! Per rendering-model-v2 spec § "SceneCompositor — composed output
//! pass", this will eventually own the z-ordered scene stack
//! (root + non-redirected windows + COW + cursor), the per-output
//! scanout image rotation with buffer-age damage history, and the
//! single composed pass that produces a scanout-ready image. For
//! Stage 1b it's an empty marker — the real shape lands in Stage 2.

#[allow(dead_code)]
pub(crate) struct SceneCompositor;

impl SceneCompositor {
    pub(crate) const fn stub() -> Self {
        Self
    }
}
