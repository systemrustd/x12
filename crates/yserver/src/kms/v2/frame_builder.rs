//! Stage 5 frame-builder Phase B sub-phase B.1: deferred per-frame
//! op-list recording.
//!
//! `FrameBuilder` owns a `Closed ↔ OpenForPaint` lifecycle. Paint
//! entry points that have been ported (`composite_glyphs` in B.1)
//! append `RecordedOp`s; a close trigger (Invariant M2 / M3, the
//! existing `get_image` / PRESENT-completion sync points, a timeout,
//! shutdown, or a pin-set ceiling) replays the op list as ONE primary
//! command buffer, submits it via the `SubmitGroup` (cap=1, so the
//! submit auto-flushes immediately), and parks the frame's resource
//! pins on a `pending_frames` queue gated by the submit's
//! `FenceTicket`.
//!
//! Phase B spec — `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.
//! This file holds the no-Vk-required pieces (state machine, op enum,
//! pin sets, layout overlay); the recording side lives in
//! `engine.rs::FrameBuilder::close_into_cb_*` because it needs the
//! engine's CB pool + atlas + drawable-store access.

use super::platform::FenceTicket;

/// Why a frame closed. Bumped into telemetry on every close so the
/// rollout can see which trigger is dominating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// `maybe_composite` saw a ready output + dirty scene; the frame
    /// closes paint-only (compose stays separate in B.1 — folded into
    /// the frame at B.4).
    SceneCompose,
    /// Invariant M2: a non-ported paint op is about to record its own
    /// CB; close the frame first so the non-ported op sees committed
    /// `Drawable::storage.current_layout` + `last_render_ticket`.
    NonPortedPaintOp,
    /// Invariant M3: legacy scene compose is about to record; close
    /// the frame first for the same reason as M2.
    LegacyScCompose,
    /// COW PRESENT-completion semaphore got attached; the frame must
    /// close immediately so `vkGetSemaphoreFdKHR(SYNC_FD)` sees a
    /// queued signal-op (Task 6.1 yoga hang precedent).
    PresentCompletionSignal,
    /// `get_image` is about to wait on a fence; close the frame first
    /// so the readback's `ticket.wait()` observes a submitted CB.
    SyncWait,
    /// Idle / no-pageflip case. A frame open > T ms forces close to
    /// release pinned resources.
    Timeout,
    /// `KmsBackendV2::shutdown` is tearing down platform state.
    Shutdown,
    /// `max_pinned_resources_per_frame` ceiling hit (1024 default).
    PinCeiling,
}

/// `FrameBuilder` lifecycle. `Closed` is the hot path for X11 traffic
/// that doesn't touch the paint surface (event-only requests, idle).
/// `OpenForPaint` is where every recorded op accumulates between
/// the first paint and a close trigger.
///
/// Phase B's spec sketches a third state, `ClosingWithCompose`, for
/// when scene compose joins the frame. That state lands in sub-phase
/// B.4; B.1 only carries the two-state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameState {
    Closed,
    OpenForPaint,
}

#[derive(Debug)]
pub(crate) struct FrameBuilder {
    state: FrameState,
    open: Option<Box<OpenFrame>>,
    lifetime_opens: u64,
    lifetime_closes: u64,
    max_pinned_resources_per_frame: usize,
}

impl FrameBuilder {
    pub(crate) fn new() -> Self {
        Self {
            state: FrameState::Closed,
            open: None,
            lifetime_opens: 0,
            lifetime_closes: 0,
            max_pinned_resources_per_frame: 1024,
        }
    }

    pub(crate) fn state(&self) -> FrameState {
        self.state
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.state, FrameState::OpenForPaint)
    }

    pub(crate) fn lifetime_opens(&self) -> u64 {
        self.lifetime_opens
    }

    pub(crate) fn lifetime_closes(&self) -> u64 {
        self.lifetime_closes
    }

    pub(crate) fn set_max_pinned_resources_per_frame(&mut self, n: usize) {
        self.max_pinned_resources_per_frame = n.max(1);
    }

    pub(crate) fn max_pinned_resources_per_frame(&self) -> usize {
        self.max_pinned_resources_per_frame
    }
}

/// Per-frame bookkeeping. Allocated when `Closed → OpenForPaint` fires;
/// dropped on close.
#[derive(Debug)]
pub(crate) struct OpenFrame {
    pub(crate) ticket: FenceTicket,
    pub(crate) close_reason_on_open: Option<CloseReason>, // unused in B.1; reserved for B.4
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn fresh_frame_builder_is_closed_with_no_lifetime_counts() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.state(), FrameState::Closed);
        assert!(!fb.is_open());
        assert_eq!(fb.lifetime_opens(), 0);
        assert_eq!(fb.lifetime_closes(), 0);
    }

    #[test]
    fn default_pin_ceiling_is_1024() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.max_pinned_resources_per_frame(), 1024);
    }

    #[test]
    fn set_max_pinned_resources_clamps_to_at_least_one() {
        let mut fb = FrameBuilder::new();
        fb.set_max_pinned_resources_per_frame(0);
        assert_eq!(fb.max_pinned_resources_per_frame(), 1);
        fb.set_max_pinned_resources_per_frame(42);
        assert_eq!(fb.max_pinned_resources_per_frame(), 42);
    }

    #[test]
    fn close_reason_has_eight_variants_for_b1() {
        fn _exhaustive(r: CloseReason) -> &'static str {
            match r {
                CloseReason::SceneCompose => "scene_compose",
                CloseReason::NonPortedPaintOp => "non_ported_paint_op",
                CloseReason::LegacyScCompose => "legacy_sc_compose",
                CloseReason::PresentCompletionSignal => "present_completion_signal",
                CloseReason::SyncWait => "sync_wait",
                CloseReason::Timeout => "timeout",
                CloseReason::Shutdown => "shutdown",
                CloseReason::PinCeiling => "pin_ceiling",
            }
        }
        assert_eq!(_exhaustive(CloseReason::SceneCompose), "scene_compose");
    }
}

// The rest of this module — RecordedOp, FramePinSet, FrameLayoutTable,
// FrameSubmittedRecord — lands in subsequent tasks.

use ash::vk;

use super::glyph_atlas::{AtlasEntry, GlyphKey};
use super::store::DrawableId;

/// Index into `OpenFrame::pins.staging_buffers`. Saved on `RecordedOp`
/// payloads so close-time replay can fetch the right pinned buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedStagingIdx(pub(crate) u32);

/// A glyph to draw at frame-close time. Mirrors the in-tree
/// `TextGlyph` struct (`crate::kms::vk::ops::text::TextGlyph`); we hold
/// our own copy here so the recorded op is independent of the live
/// `TextGlyph` type (which the recorder consumes by reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordedTextGlyph {
    pub(crate) atlas_x: u32,
    pub(crate) atlas_y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) dst_x: i32,
    pub(crate) dst_y: i32,
}

#[derive(Debug)]
pub(crate) struct RecordedCompositeGlyphs {
    pub(crate) dst_id: DrawableId,
    pub(crate) foreground_rgba: [f32; 4],
    pub(crate) glyphs: Vec<RecordedTextGlyph>,
    pub(crate) clip_scissors: Vec<vk::Rect2D>,
    /// Damage rect to commit on close-success. Pre-computed at append
    /// time (today's `composite_glyphs` already computes the same
    /// bbox at engine.rs:3913-3922) so close-time doesn't have to
    /// re-walk the glyph list.
    pub(crate) damage_rect: Option<vk::Rect2D>,
}

#[derive(Debug)]
pub(crate) struct RecordedGlyphUpload {
    /// Pin index into the frame's staging-buffer pin vector. Replay
    /// reads the buffer handle from the pinned Arc.
    pub(crate) staging_pin_idx: PinnedStagingIdx,
    pub(crate) atlas_x: u32,
    pub(crate) atlas_y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    /// Cache-insert pair to commit on close-success (atlas's lookup
    /// becomes hit-able by this key after the frame ticket signals,
    /// but the cache entry is committed in the engine on close-success
    /// — the spec's "transactional cache insert" discipline).
    pub(crate) insert_key: GlyphKey,
    pub(crate) insert_entry: AtlasEntry,
}

/// Reserved for future ops that need an explicit cross-frame layout
/// transition. `composite_glyphs` doesn't emit any in B.1 (the text
/// pipeline's recorder embeds its own barriers via the per-call
/// `StorageTextTarget` adapter), but the variant exists so the
/// recorder skeleton in Task 11 can match exhaustively and B.2 can
/// fold ported `render_composite` / `render_fill` paths in without
/// touching this enum's variant set.
#[derive(Debug)]
pub(crate) struct RecordedLayoutTransition {
    pub(crate) drawable_id: DrawableId,
    pub(crate) src_stage: vk::PipelineStageFlags2,
    pub(crate) src_access: vk::AccessFlags2,
    pub(crate) dst_stage: vk::PipelineStageFlags2,
    pub(crate) dst_access: vk::AccessFlags2,
    pub(crate) target_layout: vk::ImageLayout,
}

#[derive(Debug)]
pub(crate) enum RecordedOp {
    CompositeGlyphs(RecordedCompositeGlyphs),
    GlyphUpload(RecordedGlyphUpload),
    LayoutTransition(RecordedLayoutTransition),
}

use std::sync::Arc;

/// Resource pins held alive across a frame. Mechanism 1 of spec
/// § "Frame-wide resource pinning". B.1 only pins `StagingBuffer`
/// clones (one per glyph upload). B.2 will extend with sync objects,
/// semaphores, and Mechanism 3 Arc'd scratch handles.
#[derive(Debug, Default)]
pub(crate) struct FramePinSet {
    pub(crate) staging_buffers: Vec<Arc<super::engine::StagingBuffer>>,
}

impl FramePinSet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn pin_staging(
        &mut self,
        staging: Arc<super::engine::StagingBuffer>,
    ) -> PinnedStagingIdx {
        let idx = u32::try_from(self.staging_buffers.len()).expect("< u32::MAX pins");
        self.staging_buffers.push(staging);
        PinnedStagingIdx(idx)
    }

    pub(crate) fn len(&self) -> usize {
        self.staging_buffers.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.staging_buffers.is_empty()
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    // No-Vk pin tests can't construct a real StagingBuffer (it owns Vk
    // handles). Pin tests here verify the bookkeeping; integration
    // tests in v2_acceptance.rs verify the real-Vk path.

    #[test]
    fn fresh_pin_set_is_empty() {
        let p = FramePinSet::new();
        assert_eq!(p.len(), 0);
        assert!(p.is_empty());
    }
}

#[cfg(test)]
mod op_tests {
    use super::*;

    #[test]
    fn recorded_op_size_is_under_256_bytes() {
        assert!(
            std::mem::size_of::<RecordedOp>() <= 256,
            "RecordedOp is {} bytes — exceeds 256-byte budget",
            std::mem::size_of::<RecordedOp>()
        );
    }

    #[test]
    fn recorded_composite_glyphs_carries_dst_glyph_list_and_clip() {
        let glyph = RecordedTextGlyph {
            atlas_x: 10,
            atlas_y: 20,
            w: 8,
            h: 16,
            dst_x: 100,
            dst_y: 200,
        };
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 640,
                height: 480,
            },
        };
        let op = RecordedCompositeGlyphs {
            dst_id: DrawableId::for_tests(1),
            foreground_rgba: [1.0, 0.0, 0.0, 1.0],
            glyphs: vec![glyph],
            clip_scissors: vec![scissor],
            damage_rect: Some(vk::Rect2D {
                offset: vk::Offset2D { x: 100, y: 200 },
                extent: vk::Extent2D {
                    width: 8,
                    height: 16,
                },
            }),
        };

        assert_eq!(op.dst_id, DrawableId::for_tests(1));
        assert_eq!(op.foreground_rgba, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(op.glyphs.len(), 1);
        assert_eq!(op.glyphs[0].atlas_x, 10);
        assert_eq!(op.glyphs[0].dst_x, 100);
        assert_eq!(op.clip_scissors.len(), 1);
        assert!(op.damage_rect.is_some());
    }

    #[test]
    fn recorded_glyph_upload_carries_staging_index_and_pending_insert() {
        let key = GlyphKey {
            font_xid: 1234,
            codepoint: 65,
        };
        let entry = AtlasEntry {
            atlas_x: 0,
            atlas_y: 32,
            w: 8,
            h: 16,
            pen_left: 0,
            pen_top: 14,
        };
        let op = RecordedGlyphUpload {
            staging_pin_idx: PinnedStagingIdx(3),
            atlas_x: 0,
            atlas_y: 32,
            w: 8,
            h: 16,
            insert_key: key,
            insert_entry: entry,
        };

        assert_eq!(op.staging_pin_idx, PinnedStagingIdx(3));
        assert_eq!(op.atlas_x, 0);
        assert_eq!(op.atlas_y, 32);
        assert_eq!(op.w, 8);
        assert_eq!(op.h, 16);
        assert_eq!(op.insert_key.font_xid, 1234);
        assert_eq!(op.insert_key.codepoint, 65);
        assert_eq!(op.insert_entry.pen_top, 14);
    }
}

use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutOverlayEntry {
    pub(crate) pre_frame_layout: vk::ImageLayout,
    pub(crate) current_in_frame_layout: vk::ImageLayout,
}

/// Per-frame layout overlay. Mutated on each `record_layout_transition`
/// from a ported paint op (none in B.1 — the text pipeline's recorder
/// embeds its own barriers — but the structure is the load-bearing
/// answer to open question 3 and lands in B.1 so B.2 can fold ported
/// `render_composite` / `render_fill` paths in without re-architecting).
///
/// Atlas image layout is tracked separately: the overlay carries a
/// single `Option<LayoutOverlayEntry>` for the atlas because there's
/// exactly one atlas per engine, and `V2GlyphAtlas::current_layout`
/// is the single source of truth that the commit step writes back.
#[derive(Debug, Default)]
pub(crate) struct FrameLayoutTable {
    pub(crate) drawables: HashMap<DrawableId, LayoutOverlayEntry>,
    pub(crate) atlas: Option<LayoutOverlayEntry>,
}

impl FrameLayoutTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// First-touch snapshot for a drawable. `pre_frame_layout` is the
    /// value the caller read out of `Drawable::storage.current_layout`
    /// at the moment of first append-in-frame.
    pub(crate) fn first_touch_drawable(
        &mut self,
        id: DrawableId,
        pre_frame_layout: vk::ImageLayout,
    ) {
        self.drawables.entry(id).or_insert(LayoutOverlayEntry {
            pre_frame_layout,
            current_in_frame_layout: pre_frame_layout,
        });
    }

    pub(crate) fn set_drawable_in_frame(&mut self, id: DrawableId, layout: vk::ImageLayout) {
        if let Some(entry) = self.drawables.get_mut(&id) {
            entry.current_in_frame_layout = layout;
        } else {
            debug_assert!(
                false,
                "set_drawable_in_frame without first_touch_drawable for {:?}",
                id
            );
        }
    }

    pub(crate) fn first_touch_atlas(&mut self, pre_frame_layout: vk::ImageLayout) {
        if self.atlas.is_none() {
            self.atlas = Some(LayoutOverlayEntry {
                pre_frame_layout,
                current_in_frame_layout: pre_frame_layout,
            });
        }
    }

    pub(crate) fn set_atlas_in_frame(&mut self, layout: vk::ImageLayout) {
        match self.atlas.as_mut() {
            Some(entry) => entry.current_in_frame_layout = layout,
            None => debug_assert!(false, "set_atlas_in_frame without first_touch_atlas"),
        }
    }

    /// Query the effective layout for `id` from the perspective of
    /// the next in-frame op that will touch it. Falls back to
    /// `storage_fallback` (the caller passes
    /// `drawable.storage.current_layout` if the drawable isn't in
    /// the overlay yet).
    pub(crate) fn current_layout_for_drawable(
        &self,
        id: DrawableId,
        storage_fallback: vk::ImageLayout,
    ) -> vk::ImageLayout {
        match self.drawables.get(&id) {
            Some(entry) => entry.current_in_frame_layout,
            None => storage_fallback,
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn first_touch_drawable_snapshots_pre_frame_and_in_frame_equal() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_drawable(DrawableId::for_tests(7), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let entry = t.drawables.get(&DrawableId::for_tests(7)).unwrap();
        assert_eq!(entry.pre_frame_layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        assert_eq!(
            entry.current_in_frame_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        );
    }

    #[test]
    fn second_touch_does_not_overwrite_pre_frame() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_drawable(DrawableId::for_tests(7), vk::ImageLayout::UNDEFINED);
        t.set_drawable_in_frame(DrawableId::for_tests(7), vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        t.first_touch_drawable(DrawableId::for_tests(7), vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        let entry = t.drawables.get(&DrawableId::for_tests(7)).unwrap();
        assert_eq!(entry.pre_frame_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(
            entry.current_in_frame_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
        );
    }

    #[test]
    fn current_layout_for_drawable_falls_back_to_storage_when_untouched() {
        let t = FrameLayoutTable::new();
        let got = t.current_layout_for_drawable(DrawableId::for_tests(8), vk::ImageLayout::PRESENT_SRC_KHR);
        assert_eq!(got, vk::ImageLayout::PRESENT_SRC_KHR);
    }

    #[test]
    fn atlas_first_touch_then_set_in_frame() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_atlas(vk::ImageLayout::UNDEFINED);
        t.set_atlas_in_frame(vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        let a = t.atlas.unwrap();
        assert_eq!(a.pre_frame_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(a.current_in_frame_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    }
}

/// Per-frame snapshot of `Drawable::last_render_ticket` taken at first
/// append-in-frame. Close-failure restores each entry; close-success
/// is a no-op (the frame ticket already overwrote the slot via
/// `store.touch_render_fence` at append-time).
#[derive(Debug, Default)]
pub(crate) struct TouchedDrawables {
    /// `None` value = drawable had no prior ticket before this frame.
    pub(crate) snapshots: HashMap<DrawableId, Option<FenceTicket>>,
}

impl TouchedDrawables {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record the first-touch pre-frame ticket. Subsequent calls on
    /// the same id are no-ops (the first snapshot is the load-bearing
    /// one — it captures the value the engine needs to restore on
    /// close-failure).
    pub(crate) fn first_touch(
        &mut self,
        id: DrawableId,
        prior_ticket: Option<FenceTicket>,
    ) {
        self.snapshots.entry(id).or_insert(prior_ticket);
    }

    pub(crate) fn len(&self) -> usize {
        self.snapshots.len()
    }
}

#[cfg(test)]
mod touched_tests {
    use super::*;

    #[test]
    fn first_touch_records_prior_ticket_only_once() {
        let mut t = TouchedDrawables::new();
        t.first_touch(DrawableId::for_tests(1), None);
        assert_eq!(t.len(), 1);
        // Subsequent calls do not overwrite (a later op on the same
        // drawable should not lose the originally-captured prior).
        t.first_touch(DrawableId::for_tests(1), None);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn separate_drawables_track_independently() {
        let mut t = TouchedDrawables::new();
        t.first_touch(DrawableId::for_tests(1), None);
        t.first_touch(DrawableId::for_tests(2), None);
        assert_eq!(t.len(), 2);
    }
}

/// Pending glyph cache inserts. `composite_glyphs`'s upload path
/// already calls `V2GlyphAtlas::pack` (shelf advance, monotonic — the
/// slot stays consumed even if the frame fails), but `insert_entry`
/// (cache commit) is deferred here. Close-success drains this and
/// calls `V2GlyphAtlas::insert_entry` on the atlas; close-failure
/// drops the list — the slot leaks but the cache stays consistent
/// (next paint re-packs).
#[derive(Debug, Default)]
pub(crate) struct PendingGlyphInserts {
    pub(crate) entries: Vec<(GlyphKey, AtlasEntry)>,
}

impl PendingGlyphInserts {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, key: GlyphKey, entry: AtlasEntry) {
        self.entries.push((key, entry));
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod glyph_insert_tests {
    use super::*;

    #[test]
    fn fresh_is_empty() {
        assert_eq!(PendingGlyphInserts::new().len(), 0);
    }

    #[test]
    fn push_appends_in_order() {
        let mut p = PendingGlyphInserts::new();
        p.push(
            GlyphKey { font_xid: 1, codepoint: 65 },
            AtlasEntry { atlas_x: 0, atlas_y: 0, w: 8, h: 12, pen_left: 0, pen_top: 0 },
        );
        p.push(
            GlyphKey { font_xid: 1, codepoint: 66 },
            AtlasEntry { atlas_x: 8, atlas_y: 0, w: 8, h: 12, pen_left: 0, pen_top: 0 },
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p.entries[0].0.codepoint, 65);
        assert_eq!(p.entries[1].0.codepoint, 66);
    }
}
