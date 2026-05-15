//! `DrawableStore` stub — Stage 1b scaffolding.
//!
//! Per rendering-model-v2 spec § "DrawableStore — drawable storage +
//! lifetime", this will eventually own every drawable's storage
//! keyed by DrawableId, refcount + retirement-generation tracking,
//! and the two damage regions (presentation + protocol) per storage.
//! For Stage 1b it's an empty marker — the real shape lands in
//! Stage 2.

#[allow(dead_code)]
pub(crate) struct DrawableStore;

impl DrawableStore {
    pub(crate) const fn stub() -> Self {
        Self
    }
}
