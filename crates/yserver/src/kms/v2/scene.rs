//! `SceneCompositor` — composed output pass (Stage 2d MVP).
//!
//! Per rendering-model-v2 spec § "SceneCompositor" and Stage 2
//! plan substage 2d. Owns the blit pipeline (reuses v1's
//! `CompositorPipeline` — same shaders, same descriptor layout,
//! same sampler), per-output descriptor-pool rings, the
//! scene-structure dirty flag, and the per-output pending-ack
//! queues that thread snapshot/ack through the I6b page-flip
//! retirement path.
//!
//! Stage 2d MVP scope:
//!
//! - **Full-redraw every tick.** Buffer-age clipping is Stage 2e.
//!   Stage 2d still records the damage snapshots so 2e is a
//!   smaller diff, but the actual compose draws every scene
//!   entry every frame.
//! - **Single-output preferred.** The code loops over all
//!   outputs, but only the single-output xfce-on-bee path is
//!   exercised. Multi-output flip ordering is risk-listed in
//!   the Stage 2 plan (Risk 20).
//! - **No HW cursor plane.** Per I7 the cursor parks; Stage 5
//!   reintroduces it as a SceneCompositor strategy choice. For
//!   Stage 2d the cursor is skipped from the scene entirely —
//!   cursor rendering needs a small cursor pixmap which Stage 3
//!   will allocate alongside `create_cursor` wiring.
//! - **Manual-redirected backings skipped automatically.** They
//!   carry `scene_participating = false` per Stage 2b's
//!   `RedirectedBacking` default, so the scene-assembly loop
//!   already excludes them.
//! - **bg_pixel only.** Root background is the
//!   `vkCmdBeginRendering` clear color; `bg_pixmap` (which
//!   needs a sample-from-pixmap into root) waits for Stage 3.
//!
//! Compose flow (per [`SceneCompositor::tick`] call):
//!
//! 1. For each output, if `acquire_scanout_bo` returns `None`
//!    (all BOs in flight), skip — next core-loop iteration retries.
//! 2. Walk `core.top_level_order`, look up each window's
//!    drawable in `store`, build a `CompositeDraw` list.
//! 3. Peek presentation damage on each contributing drawable;
//!    record the snapshot keyed by drawable id for later ack.
//! 4. Call `kms::vk::compositor::record_and_present_composite`
//!    — records the compose CB into the scanout BO's
//!    pre-allocated `vk_transfer.command_buffer`, submits with
//!    `signalSemaphore = bo.vk_semaphore`, exports the sync_file
//!    fd, atomic-flips with explicit IN_FENCE_FD. v1's helper
//!    handles all of this; v2 just builds the scene + reuses
//!    the helper.
//! 5. Push a `PendingAck` onto the output's queue, advance
//!    `scene_structure_dirty = false`.
//!
//! [`SceneCompositor::handle_page_flip_complete`] then ack's
//! the captured snapshots after KMS retires the matching BO.

#![allow(
    dead_code,
    reason = "SceneCompositor primitives are consumed across Stages 2d–2e"
)]

use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, OnceLock},
};

use ash::vk;

use super::{
    platform::{FenceTicket, PlatformBackend},
    store::{DamageSnapshot, DrawableKind, DrawableStore, RegionSet},
    telemetry::Telemetry,
};
use crate::kms::{
    core::KmsCore,
    scheduler::composite_pool_ring::CompositePoolRing,
    vk::{
        compositor::{CompositeDraw, CompositeScene, PresentError},
        pipeline::{CompositePushConsts, CompositorPipeline, MAX_DESCRIPTOR_SETS_PER_FRAME},
        scanout::{BoPhase, ScanoutBo},
    },
};

// ────────────────────────────────────────────────────────────────
// Per-output state
// ────────────────────────────────────────────────────────────────

/// Per-output pending-ack ledger. Each entry corresponds to one
/// in-flight compose; popped front on page-flip-complete.
struct PendingAck {
    bo_idx: usize,
    generation: u64,
    /// Snapshots taken at tick entry, one per source drawable
    /// that contributed to the compose. Ack'd against the
    /// store's live presentation damage on flip retirement.
    drawable_snapshots: Vec<DamageSnapshot>,
    /// Engine fence ticket for the source drawables touched by
    /// the compose. Per cross-cutting §5: every consumer that
    /// reads OR writes a drawable touches the ticket; this is
    /// the compose-read side.
    ticket: Option<FenceTicket>,
    /// Output-level damage submitted in this frame (codex
    /// round 2 point 1). Subtracted from
    /// `output.scene_structure_damage` +
    /// `output.pending_repaint_after_failed_submit` on
    /// retirement. Damage that arrived between submit and
    /// retirement is NOT in this snapshot — it survives.
    submitted_output_damage: RegionSet,
    submitted_scene_structure_damage: RegionSet,
    submitted_failed_repaint: RegionSet,
}

struct FailedSubmitBo {
    bo_idx: usize,
    pool_slot: usize,
    ticket: FenceTicket,
}

/// Ring of recent output-damage regions keyed by generation.
/// Depth = max(scanout_bo_count) + 1 per Stage 2 plan
/// cross-cutting §"BufferAgeRing".
pub(crate) struct BufferAgeRing {
    entries: VecDeque<(u64, RegionSet)>,
    depth: usize,
}

impl BufferAgeRing {
    fn new(depth: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(depth + 1),
            depth,
        }
    }

    /// Push `(gen, region)`. Trims to `depth` entries.
    fn push(&mut self, generation: u64, region: RegionSet) {
        self.entries.push_back((generation, region));
        while self.entries.len() > self.depth {
            self.entries.pop_front();
        }
    }

    /// Check whether every generation in `(last_gen+1, frame_gen)`
    /// (exclusive on both sides — those are the intervening
    /// generations between the BO's last present and the
    /// frame we're about to render) is in the ring.
    fn contains_all(&self, last_gen: u64, frame_gen: u64) -> bool {
        if frame_gen <= last_gen {
            return true; // shouldn't happen but bail safe
        }
        let want_count = (frame_gen - last_gen - 1) as usize;
        if want_count == 0 {
            // No intervening frames; the BO's content + current
            // damage covers it.
            return true;
        }
        let mut found = 0usize;
        for &(g, _) in &self.entries {
            if g > last_gen && g < frame_gen {
                found += 1;
            }
        }
        found >= want_count
    }

    /// Union all damage regions in `(last_gen+1, frame_gen)` into
    /// `dst`.
    fn union_history_into(&self, last_gen: u64, frame_gen: u64, dst: &mut RegionSet) {
        for (g, r) in &self.entries {
            if *g > last_gen && *g < frame_gen {
                dst.union_with(r);
            }
        }
    }
}

struct OutputSceneState {
    output_idx: usize,
    pool_ring: CompositePoolRing,
    /// Slots map: pending_ack[i] is using descriptor-pool slot
    /// `pool_slots[i]`. Released to the ring on flip retirement.
    pool_slots: VecDeque<usize>,
    pending_acks: VecDeque<PendingAck>,
    /// GPU-submitted frames whose atomic commit was rejected.
    /// Keep both BO and descriptor-pool slot alive until the
    /// compose fence signals, then recycle them locally because
    /// no page-flip-complete will arrive for these frames.
    failed_submit_bos: VecDeque<FailedSubmitBo>,
    /// Buffer-age damage history (Stage 2e).
    damage_history: BufferAgeRing,
    /// Monotonic per-output generation. Advances only on a
    /// successful flip (transactional commit per codex round 2
    /// point 2).
    current_generation: u64,
    /// Scene-structure damage in output coords. Accumulated by
    /// `mark_scene_structure_damage(region)`; subtracted on
    /// retirement using the snapshot captured at submit time.
    scene_structure_damage: RegionSet,
    /// Repaint pending from prior failed submit/flip. Folded
    /// into the next tick's output damage.
    pending_repaint_after_failed_submit: RegionSet,
    /// Output extent — cached for full-output fallback regions.
    output_extent: vk::Extent2D,
    /// Backoff after atomic-commit failures. Without this, a failed
    /// commit can be retried once per core-loop iteration and flood
    /// KMS/RADV until the GPU context is lost.
    next_submit_retry_at: Option<std::time::Instant>,
}

// ────────────────────────────────────────────────────────────────
// SceneCompositor
// ────────────────────────────────────────────────────────────────

pub(crate) struct SceneCompositor {
    inner: Option<SceneCompositorInner>,
    /// Stage 2d's coarse scene-structure dirty bit. Set by any
    /// map/unmap/configure/restack/redirect-state/cursor-pos
    /// change. Cleared at tick end. Stage 2e narrows to a
    /// per-region scene_structure_damage `RegionSet`.
    pub(crate) scene_structure_dirty: bool,
}

struct SceneCompositorInner {
    vk: Arc<crate::kms::vk::device::VkContext>,
    pipeline: CompositorPipeline,
    outputs: Vec<OutputSceneState>,
    /// Stage 3f.8: software cursor sprite. Registered once at
    /// backend init via `register_cursor`; appended to the scene
    /// draw list at top-of-z by `build_scene`. `None` until
    /// registered (test fixtures don't bother). The real cursor
    /// theme + `define_cursor` wiring stays Stage 4 territory; this
    /// is just a default-arrow fallback so hardware smoke has
    /// visible pointer feedback.
    cursor: Option<CursorEntry>,
    /// Stage 3f.8: cursor position at the previous build_scene
    /// call, in root-space output coords. Carried so the next
    /// tick can damage the OLD cursor rect (otherwise buffer-age
    /// clipped/LOAD path leaves the prior cursor pixels in the
    /// scanout BO — visible as a "trail" while the pointer
    /// moves). Updated to the current cursor position every time
    /// build_scene emits a cursor draw entry.
    cursor_prev_pos: Option<(i32, i32)>,
    /// Stage 4d: Composite Overlay Window scene entry. `Some`
    /// once `KmsBackendV2::get_overlay_window` has allocated the
    /// COW storage and registered it here; `None` until then or
    /// after the final `release_overlay_window`. Appended by
    /// `build_scene` ABOVE all top-levels but BELOW the cursor
    /// per the Stage 4 plan §4d layering items 3 + 4. Stub
    /// fixture (no Vk) leaves this `None` — `register_cow` is a
    /// no-op when `inner` is `None`.
    cow: Option<super::store::DrawableId>,
}

/// Stage 3f.8 cursor sprite registration. The sprite lives as a
/// regular [`DrawableStore`] entry (a `Pixmap` kind with a synthetic
/// xid) so its lifetime + Vk-handle destruction flow through the
/// same paths as any other drawable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorEntry {
    pub(crate) id: super::store::DrawableId,
    pub(crate) extent: vk::Extent2D,
    pub(crate) hot_x: i16,
    pub(crate) hot_y: i16,
}

struct SceneBuild {
    scene: CompositeScene,
    snapshots: Vec<DamageSnapshot>,
    sampled_ids: Vec<super::store::DrawableId>,
    projected_damage: RegionSet,
    new_cursor_pos: Option<(i32, i32)>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SceneError {
    #[error("vk pipeline init: {0}")]
    PipelineInit(crate::kms::vk::pipeline::PipelineError),
    #[error("vk: {0:?}")]
    Vk(vk::Result),
    #[error("scene compositor in stub mode (no Vk)")]
    NoVk,
    #[error("compositor present failed: {0}")]
    Present(PresentError),
}

impl From<PresentError> for SceneError {
    fn from(e: PresentError) -> Self {
        SceneError::Present(e)
    }
}

impl SceneCompositor {
    /// Production constructor. Builds the blit pipeline (reuses
    /// v1's CompositorPipeline — same shaders, same descriptor
    /// layout) and one descriptor-pool ring per output.
    ///
    /// # Errors
    ///
    /// `PipelineInit` on shader / pipeline build failure;
    /// `Vk(...)` on descriptor-pool init.
    pub(crate) fn new(platform: &PlatformBackend) -> Result<Self, SceneError> {
        let vk = platform.vk().ok_or(SceneError::NoVk)?.clone();
        let pipeline = CompositorPipeline::new(Arc::clone(&vk), vk::Format::B8G8R8A8_UNORM)
            .map_err(SceneError::PipelineInit)?;
        let mut outputs = Vec::with_capacity(platform.outputs.len());
        for (i, layout) in platform.outputs.iter().enumerate() {
            let ring = CompositePoolRing::new(Arc::clone(&vk), MAX_DESCRIPTOR_SETS_PER_FRAME)
                .map_err(SceneError::Vk)?;
            // Ring depth = max BO count + 1. Scanout pools are
            // 3 deep (matches v1); +1 buys edge safety per Stage 2
            // plan cross-cutting §"BufferAgeRing".
            let bo_depth = platform
                .scanout_pools
                .get(i)
                .and_then(|p| p.as_ref().map(|pp| pp.bos.len()))
                .unwrap_or(3);
            outputs.push(OutputSceneState {
                output_idx: i,
                pool_ring: ring,
                pool_slots: VecDeque::with_capacity(4),
                pending_acks: VecDeque::with_capacity(4),
                failed_submit_bos: VecDeque::with_capacity(4),
                damage_history: BufferAgeRing::new(bo_depth + 1),
                current_generation: 0,
                scene_structure_damage: RegionSet::new(),
                pending_repaint_after_failed_submit: RegionSet::new(),
                output_extent: vk::Extent2D {
                    width: u32::from(layout.width),
                    height: u32::from(layout.height),
                },
                next_submit_retry_at: None,
            });
        }
        Ok(Self {
            inner: Some(SceneCompositorInner {
                vk,
                pipeline,
                outputs,
                cursor: None,
                cursor_prev_pos: None,
                cow: None,
            }),
            scene_structure_dirty: true,
        })
    }

    /// Stage 3f.8: register the software cursor sprite after the
    /// backend has uploaded its pixel data. Idempotent — a later
    /// `define_cursor` flow (Stage 4) can swap the entry. Drops to
    /// a no-op on the stub fixture.
    pub(crate) fn register_cursor(&mut self, entry: CursorEntry) {
        if let Some(inner) = self.inner.as_mut() {
            inner.cursor = Some(entry);
            self.scene_structure_dirty = true;
        }
    }

    /// Stage 4d — register the Composite Overlay Window scene
    /// entry. Called by `KmsBackendV2::get_overlay_window` after
    /// allocating screen-extent storage for the COW xid. The
    /// scene appends a draw entry for this id ABOVE top-levels +
    /// BELOW the cursor on every subsequent `build_scene`.
    /// Marks scene structure dirty so the next tick picks up the
    /// new layer. No-op on the stub fixture (no Vk).
    pub(crate) fn register_cow(&mut self, id: super::store::DrawableId) {
        if let Some(inner) = self.inner.as_mut() {
            inner.cow = Some(id);
            self.scene_structure_dirty = true;
        }
    }

    /// Stage 4d — clear the Composite Overlay Window scene
    /// entry. Called by `KmsBackendV2::release_overlay_window`
    /// when the COW refcount falls to zero. Subsequent
    /// `build_scene` calls omit the COW layer; the storage drop
    /// itself is handled by the backend's `store.decref`. No-op
    /// on the stub fixture.
    pub(crate) fn unregister_cow(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.cow = None;
            self.scene_structure_dirty = true;
        }
    }

    /// Test fixture / Stage-1b-era stub. Construct via
    /// `SceneCompositor::stub()` so the `KmsBackendV2::for_tests`
    /// path doesn't need Vk.
    pub(crate) fn stub() -> Self {
        Self {
            inner: None,
            scene_structure_dirty: false,
        }
    }

    /// Whether the scene has a live blit pipeline. Tests use
    /// this to skip Vk-only assertions.
    pub(crate) fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    /// Mark the scene as needing a redraw. Cheap bool flip;
    /// callable from any mutation path that wants the next tick
    /// to inspect drawable/cursor damage. This deliberately does
    /// NOT add output damage: protocol paint is already represented
    /// by per-drawable presentation damage, and cursor motion is
    /// projected by `build_scene`.
    pub(crate) fn wake_for_damage(&mut self) {
        self.scene_structure_dirty = true;
    }

    /// Mark scene structure as changed. This is the coarse fallback
    /// for map/unmap/configure/restack/redirect/root-background
    /// transitions where old/new visibility cannot yet be expressed
    /// as a narrower rect.
    pub(crate) fn mark_scene_structure_dirty(&mut self) {
        self.scene_structure_dirty = true;
        if let Some(inner) = self.inner.as_mut() {
            for o in &mut inner.outputs {
                let extent = o.output_extent;
                o.scene_structure_damage.add(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent,
                });
            }
        }
    }

    /// Region-precise scene-structure damage (Stage 3+).
    pub(crate) fn mark_scene_structure_damage_rect(&mut self, output_idx: usize, r: vk::Rect2D) {
        self.scene_structure_dirty = true;
        if let Some(inner) = self.inner.as_mut()
            && let Some(o) = inner.outputs.get_mut(output_idx)
        {
            o.scene_structure_damage.add(r);
        }
    }

    /// Stage 4c.1 — rect-precise scene-structure damage where the
    /// caller doesn't know which output(s) a screen-/output-coord
    /// rect intersects. Each input rect is intersected against every
    /// output's extent and (if non-empty) added to that output's
    /// `scene_structure_damage`. Mirrors the singular
    /// `mark_scene_structure_damage_rect` setter but applies to all
    /// outputs with output-extent clipping, the dual of
    /// `add_projected_damage` (output-coord input rather than
    /// storage-local projection).
    ///
    /// In the Stage-4 single-output deployment, output origin is
    /// (0, 0) so "screen-coord" and "output-local-coord" coincide;
    /// this clip is just "drop the bits that fall off the right /
    /// bottom edge".
    pub(crate) fn mark_scene_structure_damage_rects(&mut self, rects: &[vk::Rect2D]) {
        self.scene_structure_dirty = true;
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        dispatch_clip_rects_to_outputs(
            inner
                .outputs
                .iter_mut()
                .map(|o| (o.output_extent, &mut o.scene_structure_damage)),
            rects,
        );
    }

    /// Drain in-flight compose work before tear-down. Best-effort
    /// — `device_wait_idle` is the safe fallback the platform
    /// uses anyway. Releases descriptor-pool slots so the
    /// pool-ring's Drop doesn't fire while slots are still in use.
    pub(crate) fn drain_all(&mut self, _platform: &PlatformBackend) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        for o in &mut inner.outputs {
            // Release all in-use slots — the pool ring's Drop
            // does device_wait_idle anyway.
            while let Some(slot) = o.pool_slots.pop_front() {
                o.pool_ring.release(slot);
            }
            o.pending_acks.clear();
        }
    }

    /// Compose a frame per output. Each output that has a free
    /// BO produces one atomic flip. Returns the number of
    /// outputs that successfully submitted (0 if everything was
    /// stalled / not dirty / no scene entries).
    ///
    /// # Errors
    ///
    /// Per-output failures don't abort the loop; they're logged
    /// and the next output is attempted. Top-level Err means
    /// the platform was unusable (no Vk).
    pub(crate) fn tick(
        &mut self,
        core: &KmsCore,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        windows_v2: &super::backend::WindowsV2Map,
        telemetry: &mut Telemetry,
    ) -> Result<usize, SceneError> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(SceneError::NoVk);
        };
        if platform.renderer_failed {
            return Ok(0);
        }
        let n_outputs = inner.outputs.len();
        let mut composed = 0usize;
        for output_idx in 0..n_outputs {
            match tick_one_output(
                inner, output_idx, core, store, platform, windows_v2, telemetry,
            ) {
                Ok(true) => composed += 1,
                Ok(false) => {} // skipped (no BO / empty scene)
                Err(e) => {
                    log::warn!(
                        "v2 scene tick: output {output_idx} compose failed: {e}; continuing",
                    );
                }
            }
        }
        if composed > 0 {
            self.scene_structure_dirty = false;
        }
        Ok(composed)
    }

    /// Handle a DRM page-flip-complete event for `output_idx`.
    /// Pops the matching pending-ack, ack's its damage snapshots
    /// against the store, releases the descriptor-pool slot,
    /// then advances the platform's BO state machine via
    /// `on_page_flip_complete`. Engine retirement happens after
    /// (driven by the backend wrapper to keep the borrows clean).
    pub(crate) fn handle_page_flip_complete(
        &mut self,
        output_idx: usize,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        let Some(retire) = platform.on_page_flip_complete(output_idx) else {
            return false;
        };
        let Some(state) = inner.outputs.get_mut(output_idx) else {
            return false;
        };
        if let Some(ack) = state.pending_acks.pop_front() {
            // Ack each per-drawable damage snapshot. Snapshots
            // from paint that landed after the tick's peek
            // survive (per I5 epoch semantics).
            for snap in ack.drawable_snapshots {
                store.ack_presentation_damage(snap);
            }
            // Subtract the submitted output-damage snapshots
            // from live state (codex round 2 point 1). Damage
            // that arrived between submit and retirement
            // (map/unmap/cursor-move while flip in flight) is
            // NOT in the snapshots and therefore survives,
            // driving the next tick.
            state
                .scene_structure_damage
                .subtract(&ack.submitted_scene_structure_damage);
            state
                .pending_repaint_after_failed_submit
                .subtract(&ack.submitted_failed_repaint);
            // Push this frame's output damage onto the
            // buffer-age history ring keyed by its generation.
            state
                .damage_history
                .push(ack.generation, ack.submitted_output_damage);
            // Release the matching pool slot.
            if let Some(slot) = state.pool_slots.pop_front() {
                state.pool_ring.release(slot);
            }
            // Commit the BO's new last_present_generation in the
            // platform (the buffer-age pick uses this on next
            // acquire).
            platform.commit_bo_present(output_idx, retire.presented_bo_idx, ack.generation);
            true
        } else {
            log::debug!(
                "v2 scene: page-flip-complete on output {output_idx} \
                 with no pending ack — startup flush or spurious event",
            );
            false
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Per-output compose tick body
// ────────────────────────────────────────────────────────────────

fn retire_failed_submit_bos(
    state: &mut OutputSceneState,
    output_idx: usize,
    platform: &mut PlatformBackend,
    vk: &crate::kms::vk::device::VkContext,
) {
    let mut remaining = VecDeque::with_capacity(state.failed_submit_bos.len());
    while let Some(failed) = state.failed_submit_bos.pop_front() {
        if failed.ticket.poll_signaled(vk) {
            platform.recycle_failed_submit_bo(output_idx, failed.bo_idx);
            state.pool_ring.release(failed.pool_slot);
            log::debug!(
                "v2 scene: recycled failed-submit output {output_idx} bo {} pool slot {}",
                failed.bo_idx,
                failed.pool_slot,
            );
        } else {
            remaining.push_back(failed);
        }
    }
    state.failed_submit_bos = remaining;
}

fn tick_one_output(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    core: &KmsCore,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    windows_v2: &super::backend::WindowsV2Map,
    telemetry: &mut Telemetry,
) -> Result<bool, SceneError> {
    // 0. **Per-output flip-pending gate.** KMS only allows one
    //    pending atomic commit per CRTC at a time; a second
    //    `drmModeAtomicCommit` while the first hasn't fired
    //    page-flip-complete returns EBUSY. Without this check
    //    the loop fires submit-after-submit faster than vblank,
    //    every second submit takes the 9b recovery path
    //    (BO invalidated, repaint deferred), nothing ever
    //    actually displays. Observed catastrophic on RADV/bee
    //    + mate + 2560x1440 (screen stays at the initial
    //    pageflip frame; bg_pixel-unset = black).
    //
    //    Skip cleanly: pending_ack non-empty means a flip is in
    //    flight. `scene_structure_dirty` stays set so the next
    //    tick (post-page-flip-complete) picks up the deferred
    //    damage. The KMS-rate cap is now structural; the rest
    //    of the pipeline can fire at whatever cadence
    //    `maybe_composite` calls us — wasted cycles bounded
    //    here.
    {
        let vk = Arc::clone(&inner.vk);
        let s = inner.outputs.get_mut(output_idx).expect("range");
        retire_failed_submit_bos(s, output_idx, platform, vk.as_ref());
        if !s.pending_acks.is_empty() {
            return Ok(false);
        }
        if let Some(deadline) = s.next_submit_retry_at
            && std::time::Instant::now() < deadline
        {
            return Ok(false);
        }
    }

    // 1. Snapshot live output state so we can fold cleanly
    //    into pending_ack later (codex round 2 point 2 —
    //    transactional generation advance).
    let (scene_structure_snap, failed_repaint_snap, frame_gen, first_frame) = {
        let s = inner.outputs.get(output_idx).expect("range");
        (
            s.scene_structure_damage.snapshot(),
            s.pending_repaint_after_failed_submit.snapshot(),
            s.current_generation + 1,
            s.current_generation == 0,
        )
    };

    // 2. Build the scene + collect projected presentation damage.
    let built = build_scene(
        core,
        store,
        windows_v2,
        output_idx,
        platform,
        inner.cursor,
        inner.cursor_prev_pos,
        inner.cow,
    );
    // Stage 3f.8 trail elimination: stash the new cursor pos so
    // the next tick can damage it as the prior rect. Only update
    // when build_scene actually emitted a cursor draw (otherwise
    // the prev pos stays as-is and we'll re-damage it next time).
    if let Some(p) = built.new_cursor_pos {
        inner.cursor_prev_pos = Some(p);
    }
    let mut output_damage = built.projected_damage;
    output_damage.union_with(&scene_structure_snap);
    output_damage.union_with(&failed_repaint_snap);
    telemetry.record_scene_entries(
        u64::try_from(built.scene.draws.len()).unwrap_or(u64::MAX),
        u64::try_from(built.scene.draws.len()).unwrap_or(u64::MAX),
    );

    // 3. Empty-damage fast path (after first frame).
    if output_damage.is_empty() && !first_frame {
        return Ok(false);
    }

    // 4. Acquire BO.
    let token = match platform.acquire_scanout_bo(output_idx) {
        Some(t) => t,
        None => return Ok(false),
    };

    // 5. Pick repaint region via buffer-age algorithm.
    let extent = inner.outputs[output_idx].output_extent;
    let repaint = if built.scene.draws.is_empty() {
        // Scene is just the bg_color clear (no top-levels, no
        // cursor, etc.). The Clipped/LOAD path would preserve
        // each BO's prior-generation content — including a
        // pre-`bg_pixel`-update black — and never re-clear it.
        // Force Full so loadOp=CLEAR paints the current
        // `bg_color` across the whole BO. Stage 4 introduces
        // root storage which makes the entry list non-empty
        // even on blank desktops; until then, "scene is the
        // clear color" is structurally common and must always
        // refresh.
        Repaint::Full(extent)
    } else {
        pick_repaint_region(
            token.last_present_generation,
            token.content_invalidated,
            frame_gen,
            &output_damage,
            &inner.outputs[output_idx].damage_history,
            extent,
        )
    };
    match repaint {
        Repaint::Full(extent) => {
            telemetry.record_full_redraw_fallback();
            telemetry.record_damage_pixels(
                u64::from(extent.width) * u64::from(extent.height),
                u64::from(extent.width) * u64::from(extent.height),
            );
        }
        Repaint::Clipped(rect) => {
            telemetry.record_damage_pixels(
                u64::from(rect.extent.width) * u64::from(rect.extent.height),
                u64::from(extent.width) * u64::from(extent.height),
            );
        }
    }

    // 6. Acquire descriptor-pool slot.
    let state = inner.outputs.get_mut(output_idx).expect("range");
    let slot = match state.pool_ring.acquire() {
        Some(s) => s,
        None => {
            log::debug!(
                "v2 scene: descriptor-pool ring exhausted for output {output_idx}; skipping tick",
            );
            return Ok(false);
        }
    };
    let descriptor_pool = state.pool_ring.pool_at(slot);

    // 7. Record + submit + flip via the v2 clipped compose path.
    let compose_ticket = platform
        .acquire_fence_ticket()
        .map_err(|e| SceneError::Present(PresentError::Vk(e)))?;
    let pool = platform
        .scanout_pools
        .get_mut(output_idx)
        .and_then(|p| p.as_mut())
        .ok_or(SceneError::NoVk)?;
    let layout = &platform.outputs[output_idx];
    let bo = pool.bos.get_mut(token.bo_idx).ok_or(SceneError::NoVk)?;
    let mut gpu_submitted = false;
    let record_start = std::time::Instant::now();
    let compose_result = record_compose_v2(
        &inner.vk,
        &platform.device,
        &layout.output,
        bo,
        &inner.pipeline,
        descriptor_pool,
        &built.scene,
        repaint,
        compose_ticket.fence(),
        &mut gpu_submitted,
    );
    let record_ns = u64::try_from(record_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    telemetry.record_compose_cb_record_ns(record_ns);
    telemetry
        .record_descriptor_allocations(u64::try_from(built.scene.draws.len()).unwrap_or(u64::MAX));

    let state = inner.outputs.get_mut(output_idx).expect("range");
    match compose_result {
        Ok(()) => {
            state.next_submit_retry_at = None;
            for id in &built.sampled_ids {
                store.touch_render_fence(*id, compose_ticket.clone());
            }
            state.pool_slots.push_back(slot);
            state.pending_acks.push_back(PendingAck {
                bo_idx: token.bo_idx,
                generation: frame_gen,
                drawable_snapshots: built.snapshots,
                ticket: Some(compose_ticket),
                submitted_output_damage: output_damage,
                submitted_scene_structure_damage: scene_structure_snap,
                submitted_failed_repaint: failed_repaint_snap,
            });
            state.current_generation = frame_gen;
            Ok(true)
        }
        Err(e) => {
            match &e {
                PresentError::Io(_) => {
                    if gpu_submitted {
                        for id in &built.sampled_ids {
                            store.touch_render_fence(*id, compose_ticket.clone());
                        }
                    }
                    // 9b — atomic commit failed after queue
                    // submit succeeded. BO contents indeterminate.
                    platform.invalidate_bo(output_idx, token.bo_idx);
                    telemetry.record_missed_pageflip();
                    log::warn!(
                        "v2 scene: atomic commit failed for output {output_idx} \
                         (bo {}): {e}; BO invalidated",
                        token.bo_idx,
                    );
                }
                _ => {
                    // 9a — queue submit failed. BO not written.
                    log::warn!(
                        "v2 scene: queue submit failed for output {output_idx} \
                         (bo {}): {e}",
                        token.bo_idx,
                    );
                }
            }
            // Both failure paths fold repaint forward and do NOT
            // push a pending_ack or advance current_generation.
            // If the GPU submission happened, keep the scanout BO
            // and descriptor-pool slot alive until the compose
            // fence signals: KMS rejected the flip, so no page-flip
            // event will retire those resources for us.
            // Re-borrow `state` after the platform.invalidate_bo
            // call (which took &mut platform).
            let state = inner.outputs.get_mut(output_idx).expect("range");
            // TODO(stage-5 perf): the 100 ms commit-retry back-off is
            // hardcoded. Empirically picked to be wide enough that
            // RADV/amdgpu releases pinned resources between attempts
            // (16 ms / one vblank was too tight under the ENOMEM
            // storm). Should become a tunable + observable via
            // telemetry (e.g. `commit_retry_backoff_ms` counter) so
            // per-driver tuning is possible without code edits.
            state.next_submit_retry_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            if let Some(br) = output_damage.bounding_rect() {
                state.pending_repaint_after_failed_submit.add(br);
            }
            if gpu_submitted {
                state.failed_submit_bos.push_back(FailedSubmitBo {
                    bo_idx: token.bo_idx,
                    pool_slot: slot,
                    ticket: compose_ticket,
                });
            } else {
                state.pool_ring.release(slot);
                platform.recycle_failed_submit_bo(output_idx, token.bo_idx);
            }
            Err(SceneError::Present(e))
        }
    }
}

/// Pick the repaint region for the upcoming compose. Returns
/// `Repaint::Full` for fallback paths and `Repaint::Clipped` for
/// the buffer-age steady state.
fn pick_repaint_region(
    bo_last_gen: Option<u64>,
    bo_invalidated: bool,
    frame_gen: u64,
    current_damage: &RegionSet,
    history: &BufferAgeRing,
    extent: vk::Extent2D,
) -> Repaint {
    if bo_invalidated {
        return Repaint::Full(extent);
    }
    let Some(last) = bo_last_gen else {
        return Repaint::Full(extent);
    };
    if !history.contains_all(last, frame_gen) {
        return Repaint::Full(extent);
    }
    let mut repaint = current_damage.clone();
    history.union_history_into(last, frame_gen, &mut repaint);
    match repaint.bounding_rect() {
        Some(r) if r.extent.width > 0 && r.extent.height > 0 => Repaint::Clipped(r),
        _ => Repaint::Full(extent),
    }
}

#[derive(Debug, Clone, Copy)]
enum Repaint {
    /// Full-output redraw with `loadOp=CLEAR`. Fallback path.
    Full(vk::Extent2D),
    /// Damaged-region-only redraw with `loadOp=LOAD`. The
    /// rectangle is the bounding box of the buffer-age repaint
    /// set — Stage 5 may split per-rect for tighter clipping.
    Clipped(vk::Rect2D),
}

fn all_zero(c: [f32; 4]) -> bool {
    c[0] == 0.0 && c[1] == 0.0 && c[2] == 0.0 && c[3] == 0.0
}

fn debug_scene_walk_xids() -> &'static HashSet<u32> {
    static XIDS: OnceLock<HashSet<u32>> = OnceLock::new();
    XIDS.get_or_init(|| {
        std::env::var("YSERVER_V2_SCENE_WALK_XIDS")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .filter_map(|part| {
                        let token = part.trim();
                        if token.is_empty() {
                            return None;
                        }
                        let hex = token
                            .strip_prefix("0x")
                            .or_else(|| token.strip_prefix("0X"))
                            .unwrap_or(token);
                        u32::from_str_radix(hex, 16)
                            .ok()
                            .or_else(|| token.parse::<u32>().ok())
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn debug_scene_walk_all() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("YSERVER_V2_SCENE_WALK_ALL").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn scene_walk_debug_enabled_for(host_xid: u32) -> bool {
    debug_scene_walk_all() || debug_scene_walk_xids().contains(&host_xid)
}

/// Walk window tree, build the per-output scene + collect damage
/// snapshots.
///
/// Stage 3f.6 lifted the Stage 2d "top-level only" simplification:
/// the recurse below walks each top-level → mapped + scene-
/// participating descendants, accumulating parent offsets into
/// absolute (root-space) coords before projecting onto the output.
/// xterm / xclock / any real app that paints into a child window
/// needs this — the bare top-level traversal showed only the
/// parent's (typically unpainted) storage on scanout.
///
/// Still-deferred simplifications:
/// - Skip the root storage entirely — bg_pixel is the clear color
///   (`scene.bg_color`). `bg_pixmap` would need a sample-from-pixmap
///   that uses the same blit pipeline as windows, deferred to
///   Stage 4 alongside the rest of the root content pipeline.
/// - Sibling z-order between children of the same parent is
///   HashMap-iteration-order (windows_v2's underlying
///   `HashMap<u32, WindowGeometryV2>`). Proper stack-order tracking
///   is post-3f.6. Most real apps (xterm, xclock) have one child
///   per parent so the ordering rarely matters at Stage 3.
/// - Cursor: Stage 3f.8 appends a default-arrow sprite at top of
///   z when `cursor` is `Some`. Real theme support + per-window
///   `define_cursor` wiring stays Stage 4.
fn build_scene(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows_v2: &super::backend::WindowsV2Map,
    output_idx: usize,
    platform: &PlatformBackend,
    cursor: Option<CursorEntry>,
    cursor_prev_pos: Option<(i32, i32)>,
    cow: Option<super::store::DrawableId>,
) -> SceneBuild {
    let bg = [0.0, 0.0, 0.0, 1.0];
    let layout = &platform.outputs[output_idx];
    let layout_x0 = layout.x;
    let layout_y0 = layout.y;
    let layout_w = u32::from(layout.width);
    let layout_h = u32::from(layout.height);

    let mut draws: Vec<CompositeDraw> = Vec::new();
    let mut snapshots: Vec<DamageSnapshot> = Vec::new();
    let mut sampled_ids: Vec<super::store::DrawableId> = Vec::new();
    let mut projected = RegionSet::new();
    if let Some(id) = store.lookup(core.window_id)
        && let Some(drawable) = store.get(id)
        && drawable.scene_participating
        && matches!(drawable.kind, DrawableKind::Root)
    {
        // Stage 4c.3 — route source-storage through `redirected_target`.
        // For an Automatic-mode redirected drawable, the scene must
        // blit FROM the backing B (not the drawable's own storage).
        // Geometry stays driven by the host drawable; only the
        // sampled storage handle reroutes.
        let source_id = store.redirected_target(id).unwrap_or(id);
        if let Some(source) = store.get(source_id)
            && source.storage.image_view != vk::ImageView::null()
        {
            #[allow(clippy::cast_precision_loss)]
            let dst_origin = [-(layout_x0 as f32), -(layout_y0 as f32)];
            #[allow(clippy::cast_precision_loss)]
            let dst_size = [
                drawable.storage.extent.width as f32,
                drawable.storage.extent.height as f32,
            ];
            draws.push(CompositeDraw {
                // Root scene draw — sample-side view carries the
                // format/depth-aware swizzle (depth-24 → α=ONE).
                // See `Storage::sample_view` for why scene draws
                // MUST NOT bind `image_view` directly.
                image_view: source.storage.sample_view,
                dst_origin,
                dst_size,
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: false,
            });
            sampled_ids.push(source_id);
            if let Some(snap) = store.peek_presentation_damage(source_id) {
                for r in snap.region.rects() {
                    add_projected_damage(
                        &mut projected,
                        *r,
                        -layout_x0,
                        -layout_y0,
                        layout_w,
                        layout_h,
                    );
                }
                snapshots.push(snap);
            }
        }
    }
    // Stage 4 diagnostic: trace-level scene-walk marker. Brackets
    // the per-top-level traces emitted inside `emit_window_subtree`
    // so grep+awk can carve out one frame's worth of decisions.
    // Enable with `RUST_LOG=yserver::kms::v2::scene=trace`.
    log::trace!(
        "v2 scene_walk begin output={output_idx} top_levels={n} \
         layout=({layout_x0},{layout_y0} {layout_w}x{layout_h})",
        n = core.top_level_order.len(),
    );
    for &top_xid in &core.top_level_order {
        emit_window_subtree(
            top_xid,
            0,
            0,
            store,
            windows_v2,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            &mut draws,
            &mut snapshots,
            &mut sampled_ids,
            &mut projected,
        );
    }
    log::trace!(
        "v2 scene_walk end output={output_idx} draws={n_draws} \
         sampled={n_sampled}",
        n_draws = draws.len(),
        n_sampled = sampled_ids.len(),
    );

    // Stage 4d: append the Composite Overlay Window draw entry
    // ABOVE all top-levels but BELOW the cursor (Stage 4 plan
    // §4d scene layering items 3 + 4). Position is (0, 0)
    // absolute screen coords — projected onto the output by
    // subtracting the layout origin, exactly like root storage
    // above. `alpha_passthrough = true` because compositors
    // paint their composited result here with alpha and the
    // scene must blend it over the top-levels rather than
    // force-opaque. Skip the entry cleanly when the storage
    // isn't ready (null image_view from a stub fixture, or COW
    // was unregistered between tick frames).
    if let Some(cow_id) = cow
        && let Some(drawable) = store.get(cow_id)
        && drawable.scene_participating
        && drawable.storage.image_view != vk::ImageView::null()
    {
        #[allow(clippy::cast_precision_loss)]
        let dst_origin = [-(layout_x0 as f32), -(layout_y0 as f32)];
        #[allow(clippy::cast_precision_loss)]
        let dst_size = [
            drawable.storage.extent.width as f32,
            drawable.storage.extent.height as f32,
        ];
        // COW scene draw — bind the sample-side view so the
        // compositor's xRGB-intent paint sees α=ONE through the
        // shader (depth-24 BGRA8 padding bytes get force-opaque
        // by the swizzle). Pre-fix the IDENTITY view leaked
        // padding-α into the scene; with `alpha_passthrough=true`
        // that blended the layer below through COW, matching the
        // "wallpaper bleeds through marco/xfwm4 composited frame"
        // hardware-smoke symptom.
        let image_view = drawable.storage.sample_view;
        draws.push(CompositeDraw {
            image_view,
            dst_origin,
            dst_size,
            src_origin: [0.0, 0.0],
            src_size: [1.0, 1.0],
            alpha_passthrough: true,
        });
        sampled_ids.push(cow_id);
        if let Some(snap) = store.peek_presentation_damage(cow_id) {
            for r in snap.region.rects() {
                add_projected_damage(
                    &mut projected,
                    *r,
                    -layout_x0,
                    -layout_y0,
                    layout_w,
                    layout_h,
                );
            }
            snapshots.push(snap);
        }
    }

    // Stage 3f.8: append the cursor sprite at top of z. Coordinates
    // are output-local: `core.cursor_x` / `core.cursor_y` are
    // root-space, and we subtract the output layout origin (the
    // same projection windows go through above). `alpha_passthrough`
    // is `true` so the sprite's alpha channel actually blends
    // against the underlying composite instead of force-opaque.
    //
    // Trail elimination: also damage the PRIOR cursor rect so the
    // buffer-age clipped/LOAD path re-blits over those pixels
    // (otherwise the sprite stays at its previous position because
    // loadOp=LOAD preserves whatever the prior frame composed
    // there).
    let mut new_cursor_pos: Option<(i32, i32)> = None;
    if let Some(cur) = cursor
        && let Some(drawable) = store.get(cur.id)
        && drawable.storage.image_view != vk::ImageView::null()
    {
        let cw = i32::try_from(cur.extent.width).unwrap_or(i32::MAX);
        let ch = i32::try_from(cur.extent.height).unwrap_or(i32::MAX);
        let layout_w_i = i32::try_from(layout_w).unwrap_or(i32::MAX);
        let layout_h_i = i32::try_from(layout_h).unwrap_or(i32::MAX);

        let add_cursor_damage = |projected: &mut RegionSet, dx: i32, dy: i32| {
            // Clip the cursor rect to the output before adding to
            // damage. RegionSet uses u32 widths so negative offsets
            // need clamping; off-output rects must be skipped
            // entirely (they don't contribute to scanout damage).
            let x0 = dx.max(0);
            let y0 = dy.max(0);
            let x1 = (dx + cw).min(layout_w_i);
            let y1 = (dy + ch).min(layout_h_i);
            if x1 <= x0 || y1 <= y0 {
                return;
            }
            projected.add(vk::Rect2D {
                offset: vk::Offset2D { x: x0, y: y0 },
                extent: vk::Extent2D {
                    width: u32::try_from(x1 - x0).unwrap_or(0),
                    height: u32::try_from(y1 - y0).unwrap_or(0),
                },
            });
        };

        // Damage the previous cursor rect (if any) so prior pixels
        // get redrawn.
        if let Some((prev_x, prev_y)) = cursor_prev_pos {
            add_cursor_damage(&mut projected, prev_x, prev_y);
        }

        let dx = (core.cursor_x as i32) - i32::from(cur.hot_x) - layout_x0;
        let dy = (core.cursor_y as i32) - i32::from(cur.hot_y) - layout_y0;
        let visible = !(dx + cw <= 0 || dy + ch <= 0 || dx >= layout_w_i || dy >= layout_h_i);
        if visible {
            draws.push(CompositeDraw {
                // Cursor sprite — sample-side view so the
                // sprite's depth-32 ARGB α passes through cleanly.
                image_view: drawable.storage.sample_view,
                #[allow(clippy::cast_precision_loss)]
                dst_origin: [dx as f32, dy as f32],
                #[allow(clippy::cast_precision_loss)]
                dst_size: [cw as f32, ch as f32],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: true,
            });
            sampled_ids.push(cur.id);
            // Damage the new cursor rect so the next tick covers it
            // even when no other draws contributed.
            add_cursor_damage(&mut projected, dx, dy);
            new_cursor_pos = Some((dx, dy));
        }
    }

    let scene = CompositeScene {
        bg_color: bg,
        draws,
    };
    SceneBuild {
        scene,
        snapshots,
        sampled_ids,
        projected_damage: projected,
        new_cursor_pos,
    }
}

/// Stage 3f.6 recurse: emit a CompositeDraw entry for `host_xid` if
/// it's mapped + scene-participating + has live storage, then recurse
/// into mapped descendants with accumulated parent offsets.
///
/// Coordinates: `parent_abs_x` / `parent_abs_y` are the absolute
/// (root-space) origin of this window's parent. The window's own
/// position (`geom.x`, `geom.y`) is parent-relative per X11, so the
/// window's absolute origin is `parent_abs + geom`. We project onto
/// the output by subtracting the output's layout origin.
///
/// A child is only visible if every ancestor in the chain is mapped;
/// `unmapped` short-circuits the entire subtree (matches X11
/// MapWindow semantics — an unmapped parent hides all descendants).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn emit_window_subtree(
    host_xid: u32,
    parent_abs_x: i32,
    parent_abs_y: i32,
    store: &mut DrawableStore,
    windows_v2: &super::backend::WindowsV2Map,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    draws: &mut Vec<CompositeDraw>,
    snapshots: &mut Vec<DamageSnapshot>,
    sampled_ids: &mut Vec<super::store::DrawableId>,
    projected: &mut RegionSet,
) {
    let debug_focus = scene_walk_debug_enabled_for(host_xid);
    // Stage 4 diagnostic: trace-level scene-walk decision per window.
    // Enable with `RUST_LOG=yserver::kms::v2::scene=trace`. The
    // top-level and descendant paths share this function so the
    // single trace site covers both. Format is greppable —
    // `v2 scene_walk xid=...: ...` — for `grep v2 scene_walk` over
    // yserver-hw.log to extract just these lines.
    let Some(geom) = windows_v2.get(&host_xid) else {
        log::trace!("v2 scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows_v2");
        if debug_focus {
            log::debug!("v2 scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows_v2");
        }
        return;
    };
    if !geom.mapped {
        // X11: an unmapped window (and entire subtree) is invisible.
        log::trace!(
            "v2 scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
             geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
            parent = geom.parent,
        );
        if debug_focus {
            log::debug!(
                "v2 scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
                 geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
                parent = geom.parent,
            );
        }
        return;
    }
    let abs_x = parent_abs_x + i32::from(geom.x);
    let abs_y = parent_abs_y + i32::from(geom.y);

    // Manual-redirect subtree boundary. When a window is
    // `scene_participating=false` here, the compositor owns the
    // entire subtree's presentation (X11 Composite §285+360 —
    // Manual-mode redirect removes the window AND its descendants
    // from normal scene-out; the compositor reads the redirected
    // backing instead). Set after the per-node decision so we
    // can return *after* the SKIP trace fires (preserves the
    // existing trace shape for live debugging) and before the
    // child-recurse below.
    //
    // Assumption: post-`geom.mapped` filter, the only writer that
    // produces `scene_participating=false` is Manual-redirect
    // activation (`set_window_scene_participation(W, false)`).
    // If a third reason is added later, audit whether it also
    // wants subtree-prune semantics; the store currently tracks
    // only the bool, not the reason.
    let mut prune_subtree = false;

    // Emit a draw entry for this window if it has live storage that
    // participates in the scene.
    let lookup_id = store.lookup(host_xid);
    if lookup_id.is_none() {
        log::trace!(
            "v2 scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
             geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
        );
        if debug_focus {
            log::debug!(
                "v2 scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
                 geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
        }
    }
    if let Some(id) = lookup_id {
        // Pull diagnostic fields up front (cheap copies) so we can
        // emit a single SKIP/WILL_EMIT trace line per gate failure
        // without re-borrowing the store across log call sites.
        let drawable_snap = store.get(id).map(|d| {
            (
                d.id,
                d.kind,
                d.depth,
                d.refcount,
                d.scene_participating,
                d.storage.extent,
                d.storage.image_view == vk::ImageView::null(),
            )
        });
        if let Some((d_id, d_kind, d_depth, d_refcount, d_part, d_extent, d_view_null)) =
            drawable_snap
        {
            // Stage 4c.3 — route source-storage through `redirected_target`.
            // Automatic-mode redirected W blits FROM B; W's geometry
            // (dst_origin, dst_size, intersect test) stays driven by W's
            // own state in `windows_v2`. Only the sampled storage handle
            // reroutes. Manual-mode W's are filtered out before this
            // path (scene_participating=false), so the indirection only
            // fires for Automatic.
            let source_id = store.redirected_target(id).unwrap_or(id);
            let source_view_null = store
                .get(source_id)
                .is_none_or(|s| s.storage.image_view == vk::ImageView::null());

            // Project onto output-local coords (computed once here so
            // both the SKIP=no_intersect and WILL_EMIT trace lines can
            // include the dst rect).
            let dx = abs_x - layout_x0;
            let dy = abs_y - layout_y0;
            let win_w = i32::from(geom.width);
            let win_h = i32::from(geom.height);
            let intersects = !(dx + win_w <= 0
                || dy + win_h <= 0
                || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
                || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));

            // Pick the first failing gate and emit a single SKIP line;
            // otherwise emit WILL_EMIT. Order matches the production
            // gate ordering below so the trace mirrors the live path.
            if !d_part {
                prune_subtree = true;
            }
            let skip_reason: Option<&'static str> = if !d_part {
                Some("scene_participating=false")
            } else if !matches!(d_kind, DrawableKind::Window) {
                Some("kind!=Window")
            } else if source_view_null {
                Some("source_image_view_null")
            } else if !intersects {
                Some("no_intersect_with_output")
            } else {
                None
            };

            if let Some(reason) = skip_reason {
                log::trace!(
                    "v2 scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                     geom=({gx},{gy} {gw}x{gh}) mapped=true \
                     store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                     refcount={d_refcount} scene_participating={d_part} \
                     storage_extent={dew}x{deh} image_view_null={d_view_null} \
                     source_id={source_id:?} source_view_null={source_view_null}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "v2 scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                         geom=({gx},{gy} {gw}x{gh}) mapped=true \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?} source_view_null={source_view_null}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            } else {
                log::trace!(
                    "v2 scene_walk xid={host_xid:#x}: WILL_EMIT \
                     geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                     output=({dx},{dy} {win_w}x{win_h}) \
                     store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                     refcount={d_refcount} scene_participating={d_part} \
                     storage_extent={dew}x{deh} image_view_null={d_view_null} \
                     source_id={source_id:?}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "v2 scene_walk xid={host_xid:#x}: WILL_EMIT \
                         geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                         output=({dx},{dy} {win_w}x{win_h}) \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            }

            if d_part
                && matches!(d_kind, DrawableKind::Window)
                && let Some(source) = store.get(source_id)
                && source.storage.image_view != vk::ImageView::null()
                && intersects
            {
                // Window scene draw — bind the sample-side view
                // (format/depth-aware swizzle) instead of the
                // raw IDENTITY-swizzle attachment view. This is
                // the load-bearing fix for the "depth-24 windows
                // / COW α leak" bug: the BgraNoAlpha swizzle
                // forced α=ONE for depth-24 used to live ONLY in
                // the engine's RENDER view-cache, never on the
                // scene path. Combined with `alpha_passthrough=true`
                // below, the prior IDENTITY view leaked the
                // BGRA8 padding byte (typically 0) into the
                // shader's `src.a`, blending depth-24 windows
                // with α=0 — invisible against root, which
                // matched the post-4d.7 mate-with-compositing
                // and xfce-with-compositing hardware-smoke
                // failure shape.
                let image_view = source.storage.sample_view;
                draws.push(CompositeDraw {
                    image_view,
                    #[allow(clippy::cast_precision_loss)]
                    dst_origin: [dx as f32, dy as f32],
                    #[allow(clippy::cast_precision_loss)]
                    dst_size: [win_w as f32, win_h as f32],
                    src_origin: [0.0, 0.0],
                    src_size: [1.0, 1.0],
                    // Depth-32 ARGB windows initialize their storage to
                    // (0,0,0,0) per `default_window_init_color(32)`, so any
                    // unpainted region is transparent and must alpha-blend
                    // onto the layers below — matching X11 / Composite
                    // semantics where the root window is the opaque
                    // bottom layer and ARGB windows stack with blending.
                    // Forcing alpha to 1.0 here (the old `false` setting)
                    // turned transparent areas into opaque black, hiding
                    // mate-panel applets, control-center sidebar text,
                    // system-tray icons, and tooltips. Depth-24 sources
                    // pass through `sample_view`'s α=ONE swizzle so the
                    // shader sees α=1 from them regardless of the BGRA8
                    // padding byte — that's the scene-α fix above.
                    alpha_passthrough: true,
                });
                sampled_ids.push(source_id);
                if let Some(snap) = store.peek_presentation_damage(source_id) {
                    for r in snap.region.rects() {
                        add_projected_damage(projected, *r, dx, dy, layout_w, layout_h);
                    }
                    snapshots.push(snap);
                }
            }
        } else {
            log::trace!(
                "v2 scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                 store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
            if debug_focus {
                log::debug!(
                    "v2 scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                     store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                    x = geom.x,
                    y = geom.y,
                    w = geom.width,
                    h = geom.height,
                    depth = geom.depth,
                );
            }
        }
    }

    if prune_subtree {
        return;
    }

    // Recurse into mapped descendants in stable sibling stack order.
    let mut children: Vec<(u32, u64)> = windows_v2
        .iter()
        .filter_map(|(xid, g)| {
            if g.parent == Some(host_xid) {
                Some((*xid, g.stack_rank))
            } else {
                None
            }
        })
        .collect();
    children.sort_by_key(|(_, rank)| *rank);
    for (child_xid, _) in children {
        emit_window_subtree(
            child_xid,
            abs_x,
            abs_y,
            store,
            windows_v2,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            draws,
            snapshots,
            sampled_ids,
            projected,
        );
    }
}

/// Stage 4c.1 — for each `(extent, damage)` pair in `outputs`, clip
/// every rect in `rects` to that output's extent and (if non-empty)
/// add the clipped rect to that output's damage `RegionSet`.
///
/// Extracted from [`SceneCompositor::mark_scene_structure_damage_rects`]
/// so the dispatch + clip + accumulate wiring is unit-testable
/// without needing a live `VkContext` + `CompositorPipeline`.
fn dispatch_clip_rects_to_outputs<'a, I>(outputs: I, rects: &[vk::Rect2D])
where
    I: IntoIterator<Item = (vk::Extent2D, &'a mut RegionSet)>,
{
    for (ext, damage) in outputs {
        for r in rects {
            if let Some(clipped) = clip_rect_to_output_extent(*r, ext) {
                damage.add(clipped);
            }
        }
    }
}

/// Stage 4c.1 — intersect a rect (in output-local coords) with the
/// output's extent. Returns `None` if the intersection is empty
/// (rect lies fully outside, or input has zero width/height).
///
/// This is the output-local counterpart to `add_projected_damage`'s
/// clipping math: same rectangle-intersection arithmetic, but the
/// projection (the `+dx`/`+dy` translation that maps storage-local
/// coords into output coords) is omitted because the caller already
/// works in output coords.
fn clip_rect_to_output_extent(rect: vk::Rect2D, ext: vk::Extent2D) -> Option<vk::Rect2D> {
    let max_x = i32::try_from(ext.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(ext.height).unwrap_or(i32::MAX);
    let x0 = rect.offset.x.max(0);
    let y0 = rect.offset.y.max(0);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .min(max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .min(max_y);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    })
}

fn add_projected_damage(
    projected: &mut RegionSet,
    src: vk::Rect2D,
    dx: i32,
    dy: i32,
    layout_w: u32,
    layout_h: u32,
) {
    let layout_w_i = i32::try_from(layout_w).unwrap_or(i32::MAX);
    let layout_h_i = i32::try_from(layout_h).unwrap_or(i32::MAX);
    let x0 = (src.offset.x + dx).max(0);
    let y0 = (src.offset.y + dy).max(0);
    let x1 = (src.offset.x + dx)
        .saturating_add_unsigned(src.extent.width)
        .min(layout_w_i);
    let y1 = (src.offset.y + dy)
        .saturating_add_unsigned(src.extent.height)
        .min(layout_h_i);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    projected.add(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    });
}

// ────────────────────────────────────────────────────────────────
// v2 compose recorder — fork of v1's `record_and_present_composite`
// with buffer-age (loadOp=LOAD + per-frame scissor) support.
//
// Why fork: v1 always uses `loadOp=CLEAR` against the full BO,
// which is incompatible with buffer-age repaint (any region outside
// the clear gets clobbered to bg_color). v2 needs `LOAD` on the
// clipped path so unaltered regions retain their prior-generation
// content. The submission shape, fence handshake, and atomic-flip
// handling stay identical to v1.
// ────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn record_compose_v2(
    vk: &crate::kms::vk::device::VkContext,
    drm: &crate::drm::Device,
    output: &crate::drm::modeset::Output,
    bo: &mut ScanoutBo,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
    repaint: Repaint,
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
) -> Result<(), PresentError> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    if bo.state.phase != BoPhase::Free {
        return Err(PresentError::WrongPhase(bo.state.phase));
    }
    let fb_handle = bo.fb_handle.ok_or(PresentError::NoFb)?;
    bo.state.transition_to_recording();

    // Allocate descriptor sets — same shape as v1.
    let mut descriptors: Vec<vk::DescriptorSet> = Vec::with_capacity(scene.draws.len());
    for draw in &scene.draws {
        let layouts = [pipeline.descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let set = match unsafe { vk.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                log::warn!(
                    "v2 compose: descriptor allocation failed ({e:?}) at draw {} of {}",
                    descriptors.len(),
                    scene.draws.len(),
                );
                break;
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(draw.image_view)
            .sampler(pipeline.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { vk.device.update_descriptor_sets(&writes, &[]) };
        descriptors.push(set);
    }

    // Record.
    record_v2_command_buffer(vk, bo, pipeline, scene, &descriptors, repaint)?;

    // Submit. Same shape as v1: signal bo.vk_semaphore for the
    // KMS IN_FENCE_FD handoff; null fence.
    let cb = bo.vk_transfer.command_buffer;
    let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(bo.vk_semaphore)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submit = [vk::SubmitInfo2::default()
        .command_buffer_infos(&cb_info)
        .signal_semaphore_infos(&sig_info)];
    unsafe {
        crate::vk_count!(queue_submit2);
        crate::vk_count!(submit_compositor);
        vk.device
            .queue_submit2(vk.graphics_queue, &submit, signal_fence)?;
    }
    *gpu_submitted = true;

    // Export SYNC_FD + atomic flip — same as v1.
    let fd = bo
        .export_signaled_fd()
        .map_err(PresentError::Vk)?
        .into_raw_fd();
    bo.state.transition_to_submitted(fd);

    let mut out_fence: i32 = -1;
    match crate::drm::page_flip::submit_flip_with_fences(drm, output, fb_handle, fd, &mut out_fence)
    {
        Ok(()) => {
            if let Some(reclaimed) = bo.state.transition_to_pending(out_fence) {
                // SAFETY: `reclaimed` was inserted by
                // `transition_to_submitted` above.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            Ok(())
        }
        Err(e) => {
            if let Some(reclaimed) = bo.state.transition_to_recording_after_atomic_reject() {
                // SAFETY: same fd we just inserted.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            if out_fence >= 0 {
                // Defensive: OUT_FENCE_PTR should only be written
                // on a successful atomic commit, but close it if a
                // driver/kernel ever returns one alongside an error.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(out_fence) });
            }
            // Leave the BO non-free. The command buffer was already
            // submitted, so the caller must hold this BO until the
            // compose fence signals and then recycle it explicitly.
            Err(PresentError::Io(e))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_v2_command_buffer(
    vk: &crate::kms::vk::device::VkContext,
    bo: &ScanoutBo,
    pipeline: &CompositorPipeline,
    scene: &CompositeScene,
    descriptors: &[vk::DescriptorSet],
    repaint: Repaint,
) -> Result<(), PresentError> {
    let device = &vk.device;
    let cb = bo.vk_transfer.command_buffer;

    let (load_op, render_area, old_layout) = match repaint {
        Repaint::Full(extent) => (
            vk::AttachmentLoadOp::CLEAR,
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            },
            vk::ImageLayout::UNDEFINED,
        ),
        Repaint::Clipped(_) => (
            vk::AttachmentLoadOp::LOAD,
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: bo.width,
                    height: bo.height,
                },
            },
            // LOAD requires the previous layout to be valid; the
            // BO has been through a prior present which left it
            // at GENERAL (KMS scanout layout). Transition from
            // GENERAL → COLOR_ATTACHMENT_OPTIMAL with a full
            // memory barrier so prior writes are visible.
            vk::ImageLayout::GENERAL,
        ),
    };
    let scissor = match repaint {
        Repaint::Full(extent) => vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        },
        Repaint::Clipped(rect) => rect,
    };

    unsafe {
        device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        crate::vk_count!(begin_command_buffer);
        device.begin_command_buffer(cb, &begin)?;

        let to_color_src_access = if matches!(load_op, vk::AttachmentLoadOp::LOAD) {
            // LOAD: previous KMS scanout left the BO in GENERAL.
            // The kernel "consumed" the BO contents via the page
            // flip; we now need the GPU to read+write them. Pair
            // ALL_COMMANDS + empty source access (no prior GPU
            // work to drain — the scanout completes before the
            // pageflip event fires) with COLOR_ATTACHMENT_OUTPUT
            // + WRITE on the dst.
            vk::AccessFlags2::empty()
        } else {
            vk::AccessFlags2::empty()
        };
        let to_color = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            .src_access_mask(to_color_src_access)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(bo.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_color_arr = [to_color];
        let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(cb, &to_color_dep);

        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(bo.vk_image_view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(load_op)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: scene.bg_color,
                },
            })];
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .color_attachments(&color_attachment);
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);

        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            #[allow(clippy::cast_precision_loss)]
            width: bo.width as f32,
            #[allow(clippy::cast_precision_loss)]
            height: bo.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        crate::vk_count!(cmd_set_scissor);
        device.cmd_set_scissor(cb, 0, &[scissor]);

        #[allow(clippy::cast_precision_loss)]
        let viewport_size = [bo.width as f32, bo.height as f32];
        let mut last_pipeline: Option<vk::Pipeline> = None;
        for (i, draw) in scene.draws.iter().enumerate().take(descriptors.len()) {
            let pl = pipeline.pipeline_for(draw.alpha_passthrough);
            if last_pipeline != Some(pl) {
                crate::vk_count!(cmd_bind_pipeline);
                device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pl);
                last_pipeline = Some(pl);
            }
            let sets = [descriptors[i]];
            crate::vk_count!(cmd_bind_descriptor_sets);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.pipeline_layout,
                0,
                &sets,
                &[],
            );
            let push = CompositePushConsts {
                dst_origin: draw.dst_origin,
                dst_size: draw.dst_size,
                viewport: viewport_size,
                src_origin: draw.src_origin,
                src_size: draw.src_size,
            };
            crate::vk_count!(cmd_push_constants);
            device.cmd_push_constants(
                cb,
                pipeline.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            crate::vk_count!(cmd_draw);
            device.cmd_draw(cb, 4, 1, 0, 0);
        }

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);

        // Transition to GENERAL for KMS scanout.
        let to_scanout = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::empty())
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(bo.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_scanout_arr = [to_scanout];
        let to_scanout_dep = vk::DependencyInfo::default().image_memory_barriers(&to_scanout_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(cb, &to_scanout_dep);

        crate::vk_count!(end_command_buffer);
        device.end_command_buffer(cb)?;
    }
    let _ = render_area;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_scene_is_not_live_and_declines_tick() {
        let mut scene = SceneCompositor::stub();
        assert!(!scene.is_live());
        let core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let mut telemetry = Telemetry::new();
        let windows = super::super::backend::WindowsV2Map::new();
        let err = scene
            .tick(&core, &mut store, &mut platform, &windows, &mut telemetry)
            .expect_err("stub must reject tick");
        assert!(matches!(err, SceneError::NoVk));
    }

    #[test]
    fn mark_scene_structure_dirty_is_idempotent() {
        let mut scene = SceneCompositor::stub();
        scene.scene_structure_dirty = false;
        scene.mark_scene_structure_dirty();
        assert!(scene.scene_structure_dirty);
        scene.mark_scene_structure_dirty();
        assert!(scene.scene_structure_dirty);
    }

    /// Stage 4c.1 — the plural setter sets `scene_structure_dirty`
    /// even on the stub-mode compositor (mirrors the singular
    /// setter's early-return shape).
    #[test]
    fn mark_scene_structure_damage_rects_sets_dirty_on_stub() {
        let mut scene = SceneCompositor::stub();
        scene.scene_structure_dirty = false;
        scene.mark_scene_structure_damage_rects(&[rect(0, 0, 10, 10)]);
        assert!(scene.scene_structure_dirty);
    }

    /// Stage 4c.1 — code-quality follow-up. The plural setter test
    /// above only proves the dirty bit gets set on the stub-mode
    /// compositor (where `inner` is `None` and the dispatch for-loop
    /// is unreachable). `clip_rect_to_output_extent_handles_all_cases`
    /// only covers the helper math in isolation. Their union does
    /// NOT cover the dispatch wiring — a regression that swapped
    /// `damage.add(clipped)` for a no-op (or dropped the per-output
    /// loop entirely) would pass both tests. This test exercises
    /// the extracted `dispatch_clip_rects_to_outputs` helper that
    /// `mark_scene_structure_damage_rects` delegates to, with two
    /// synthetic outputs of different extents, and asserts that:
    ///
    /// - a rect wholly inside lands unchanged on every output;
    /// - a rect spilling off the right edge lands clipped (NOT in
    ///   its original form) on the output where it spills;
    /// - a rect that's fully outside an output is dropped for that
    ///   output but still lands on the other output if it fits there;
    /// - per-output clipping is independent (extent of output A does
    ///   not influence what lands on output B).
    #[test]
    fn dispatch_clip_rects_lands_per_output_clipped() {
        // Output 0: 800×600, Output 1: 400×400. Same input rect set.
        let ext_a = extent(800, 600);
        let ext_b = extent(400, 400);
        let mut damage_a = RegionSet::new();
        let mut damage_b = RegionSet::new();

        let inside = rect(10, 20, 100, 50); // fits both outputs
        let spilling_right = rect(700, 0, 200, 50); // spills A on right; fully outside B
        let outside_a_inside_b = rect(350, 350, 30, 30); // fits both (B clips to 50×50)
        let fully_outside = rect(2000, 2000, 50, 50); // outside both

        let rects = [inside, spilling_right, outside_a_inside_b, fully_outside];

        // Build a `Vec` of tuples so the slice carries a stable lifetime
        // for the iterator; `iter_mut` produces `(extent, &mut damage)`
        // exactly as the production callsite does.
        let mut outs: Vec<(vk::Extent2D, &mut RegionSet)> =
            vec![(ext_a, &mut damage_a), (ext_b, &mut damage_b)];
        dispatch_clip_rects_to_outputs(outs.drain(..), &rects);

        // Output A (800×600):
        //   - inside (10,20,100,50): identity
        //   - spilling_right (700,0,200,50): clipped width 200→100
        //   - outside_a_inside_b (350,350,30,30): identity (fits A)
        //   - fully_outside: dropped
        let a_rects = damage_a.rects();
        assert!(
            a_rects.contains(&inside),
            "inside rect must land unchanged on output A: {a_rects:?}",
        );
        let spilling_clipped_a = rect(700, 0, 100, 50);
        assert!(
            a_rects.contains(&spilling_clipped_a),
            "spilling rect must land CLIPPED on output A (expected {spilling_clipped_a:?}), got {a_rects:?}",
        );
        assert!(
            !a_rects.contains(&spilling_right),
            "spilling rect must NOT land in its original (unclipped) form on output A: {a_rects:?}",
        );
        assert!(
            a_rects.contains(&outside_a_inside_b),
            "rect that fits output A unchanged must land: {a_rects:?}",
        );
        assert!(
            !a_rects.iter().any(|r| r.offset.x >= 800
                || r.offset.y >= 600
                || i64::from(r.offset.x) + i64::from(r.extent.width) > 800
                || i64::from(r.offset.y) + i64::from(r.extent.height) > 600),
            "no rect on output A may spill its 800×600 extent: {a_rects:?}",
        );

        // Output B (400×400):
        //   - inside (10,20,100,50): identity
        //   - spilling_right (700,0,...): fully outside → dropped
        //   - outside_a_inside_b (350,350,30,30): clipped → (350,350,30,30) fits in 400×400
        //   - fully_outside: dropped
        let b_rects = damage_b.rects();
        assert!(
            b_rects.contains(&inside),
            "inside rect must land unchanged on output B: {b_rects:?}",
        );
        assert!(
            !b_rects.iter().any(|r| r.offset.x >= 700),
            "spilling-right (x=700) is fully outside output B and must be dropped: {b_rects:?}",
        );
        assert!(
            b_rects.contains(&outside_a_inside_b),
            "rect that fits output B unchanged must land: {b_rects:?}",
        );
        assert!(
            !b_rects
                .iter()
                .any(|r| i64::from(r.offset.x) + i64::from(r.extent.width) > 400
                    || i64::from(r.offset.y) + i64::from(r.extent.height) > 400),
            "no rect on output B may spill its 400×400 extent: {b_rects:?}",
        );
    }

    /// Stage 4c.1 — the helper clips a rect to the output's extent
    /// (offset assumed (0,0) — output-local coords). Wholly inside
    /// → identity. Partially overlapping → clipped intersection.
    /// Fully outside or zero-area → `None`.
    #[test]
    fn clip_rect_to_output_extent_handles_all_cases() {
        let ext = extent(800, 600);

        // Wholly inside — identity.
        assert_eq!(
            clip_rect_to_output_extent(rect(10, 20, 100, 50), ext),
            Some(rect(10, 20, 100, 50)),
        );

        // Right edge spills — clip width.
        assert_eq!(
            clip_rect_to_output_extent(rect(700, 0, 200, 50), ext),
            Some(rect(700, 0, 100, 50)),
        );

        // Bottom edge spills — clip height.
        assert_eq!(
            clip_rect_to_output_extent(rect(0, 500, 50, 200), ext),
            Some(rect(0, 500, 50, 100)),
        );

        // Negative offset — clamp to 0, clip width.
        assert_eq!(
            clip_rect_to_output_extent(rect(-30, -20, 100, 80), ext),
            Some(rect(0, 0, 70, 60)),
        );

        // Wholly to the right — None.
        assert_eq!(clip_rect_to_output_extent(rect(900, 0, 50, 50), ext), None);

        // Wholly below — None.
        assert_eq!(clip_rect_to_output_extent(rect(0, 700, 50, 50), ext), None);

        // Zero-width — None.
        assert_eq!(clip_rect_to_output_extent(rect(10, 10, 0, 50), ext), None);

        // Zero-height — None.
        assert_eq!(clip_rect_to_output_extent(rect(10, 10, 50, 0), ext), None);
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    fn extent(w: u32, h: u32) -> vk::Extent2D {
        vk::Extent2D {
            width: w,
            height: h,
        }
    }

    #[test]
    fn buffer_age_ring_trims_to_depth() {
        let mut ring = BufferAgeRing::new(3);
        for g in 1..=5 {
            let mut r = RegionSet::new();
            r.add(rect(0, 0, 4, 4));
            ring.push(g, r);
        }
        assert_eq!(ring.entries.len(), 3);
        // Oldest entries trimmed: 1, 2 gone; 3, 4, 5 remain.
        let gens: Vec<u64> = ring.entries.iter().map(|(g, _)| *g).collect();
        assert_eq!(gens, vec![3, 4, 5]);
    }

    #[test]
    fn buffer_age_contains_all_strict_window() {
        let mut ring = BufferAgeRing::new(4);
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        ring.push(3, r.clone());
        ring.push(4, r.clone());
        // BO last_gen=2, frame_gen=5 → intervening gens 3, 4.
        assert!(ring.contains_all(2, 5));
        // BO last_gen=2, frame_gen=6 → needs 3, 4, 5 — 5 missing.
        assert!(!ring.contains_all(2, 6));
        // No intervening gens (frame_gen == last_gen+1).
        assert!(ring.contains_all(2, 3));
    }

    #[test]
    fn pick_repaint_invalidated_bo_full_redraw() {
        let history = BufferAgeRing::new(4);
        let mut damage = RegionSet::new();
        damage.add(rect(0, 0, 10, 10));
        let p = pick_repaint_region(Some(5), true, 6, &damage, &history, extent(800, 600));
        assert!(matches!(p, Repaint::Full(_)));
    }

    #[test]
    fn pick_repaint_fresh_bo_full_redraw() {
        let history = BufferAgeRing::new(4);
        let mut damage = RegionSet::new();
        damage.add(rect(0, 0, 10, 10));
        let p = pick_repaint_region(None, false, 1, &damage, &history, extent(800, 600));
        assert!(matches!(p, Repaint::Full(_)));
    }

    #[test]
    fn pick_repaint_history_loss_full_redraw() {
        let mut history = BufferAgeRing::new(4);
        // Insert only gen 3 (missing 4, 5).
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        history.push(3, r);
        let mut damage = RegionSet::new();
        damage.add(rect(0, 0, 10, 10));
        let p = pick_repaint_region(Some(2), false, 6, &damage, &history, extent(800, 600));
        // Need 3, 4, 5 — only have 3.
        assert!(matches!(p, Repaint::Full(_)));
    }

    #[test]
    fn pick_repaint_clipped_when_history_complete() {
        let mut history = BufferAgeRing::new(4);
        let mut h3 = RegionSet::new();
        h3.add(rect(10, 10, 5, 5));
        history.push(3, h3);
        let mut h4 = RegionSet::new();
        h4.add(rect(50, 50, 8, 8));
        history.push(4, h4);
        let mut current = RegionSet::new();
        current.add(rect(0, 0, 3, 3));
        let p = pick_repaint_region(Some(2), false, 5, &current, &history, extent(800, 600));
        match p {
            Repaint::Clipped(rect) => {
                // Bounding rect should cover (0,0) to (58, 58).
                assert_eq!(rect.offset, vk::Offset2D { x: 0, y: 0 });
                assert_eq!(rect.extent.width, 58);
                assert_eq!(rect.extent.height, 58);
            }
            Repaint::Full(_) => panic!("expected clipped"),
        }
    }

    #[test]
    fn pick_repaint_clipped_empty_damage_falls_back_to_full() {
        // If everything matches up but current_damage is empty
        // and history is empty, bounding rect is None → full
        // redraw fallback.
        let history = BufferAgeRing::new(4);
        let empty = RegionSet::new();
        let p = pick_repaint_region(Some(2), false, 3, &empty, &history, extent(800, 600));
        assert!(matches!(p, Repaint::Full(_)));
    }

    // ── Stage 3f.6: subwindow scene traversal ─────────────────────

    fn alloc_stub_window(
        store: &mut DrawableStore,
        windows_v2: &mut super::super::backend::WindowsV2Map,
        xid: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        parent: Option<u32>,
        mapped: bool,
    ) {
        // for_tests_null gives null image handles; build_scene
        // rejects null views. Use a non-zero sentinel handle so the
        // traversal test exercises the recurse logic. The handle
        // never gets passed to Vk because the test never composes.
        let mut storage = super::super::store::Storage::for_tests_null(
            extent(u32::from(w), u32::from(h)),
            vk::Format::B8G8R8A8_UNORM,
        );
        // SAFETY: Vk handle types are opaque u64s; constructing a
        // sentinel doesn't touch the driver. The `is_test_stub`
        // flag on Storage means Drop won't try to destroy these.
        // Stamp both views to the same sentinel so build_scene's
        // sample-side bind (`storage.sample_view`) sees the same
        // handle the legacy tests asserted against — these stubs
        // don't exercise α swizzle, just storage-routing.
        let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
        storage.image_view = sentinel;
        storage.sample_view = sentinel;
        store
            .allocate(xid, DrawableKind::Window, 32, mapped, storage)
            .expect("stub allocate");
        windows_v2.insert(
            xid,
            super::super::backend::WindowGeometryV2 {
                x,
                y,
                width: w,
                height: h,
                depth: 32,
                mapped,
                parent,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
    }

    /// Stage 3f.6 — `build_scene` walks top-level → mapped
    /// descendants and produces draw entries in absolute coords.
    /// Top-level at (50, 60), child at (10, 20) relative → child
    /// emits at output coords (60, 80).
    #[test]
    fn build_scene_recurses_into_mapped_children() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // Top-level @ (50, 60), 200×100.
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x100,
            50,
            60,
            200,
            100,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // Child @ (10, 20) relative to top-level, 40×30.
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x101,
            10,
            20,
            40,
            30,
            Some(0x100),
            true,
        );

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            None,
            None,
            None,
        );
        let scene = built.scene;
        assert_eq!(scene.draws.len(), 2, "expected top-level + child draw");

        // Top-level at output (50, 60) since output layout origin is (0,0).
        let top = scene
            .draws
            .iter()
            .find(|d| d.dst_size[0] == 200.0 && d.dst_size[1] == 100.0)
            .expect("top-level draw present");
        assert_eq!(top.dst_origin, [50.0, 60.0]);

        // Child at absolute (60, 80) = top (50, 60) + child rel (10, 20).
        let child = scene
            .draws
            .iter()
            .find(|d| d.dst_size[0] == 40.0 && d.dst_size[1] == 30.0)
            .expect("child draw present");
        assert_eq!(child.dst_origin, [60.0, 80.0]);
    }

    /// Stage 3f.6 — unmapped parent hides the entire subtree per
    /// X11 MapWindow cascade semantics. Child stays scene-
    /// participating but doesn't render because its ancestor is
    /// unmapped.
    #[test]
    fn build_scene_unmapped_parent_hides_subtree() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x200,
            10,
            10,
            100,
            100,
            None,
            false, /* parent NOT mapped */
        );
        core.top_level_order.push(0x200);
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x201,
            0,
            0,
            50,
            50,
            Some(0x200),
            true, /* child IS mapped, but parent isn't */
        );

        let scene = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            None,
            None,
            None,
        )
        .scene;
        assert!(
            scene.draws.is_empty(),
            "unmapped parent must short-circuit subtree (got {} draws)",
            scene.draws.len()
        );
    }

    /// Stage 3f.8 — when `cursor` is `Some`, `build_scene` emits an
    /// additional top-of-z draw entry at the cursor's
    /// hot-spot-adjusted position. The entry is the LAST element of
    /// `draws` (last = topmost in z-order) and has
    /// `alpha_passthrough=true` so the sprite's alpha actually
    /// blends.
    #[test]
    fn build_scene_appends_cursor_draw_at_top_of_z() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // One mapped top-level so we can verify "cursor is on top".
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x100,
            0,
            0,
            400,
            300,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // Allocate a stub cursor storage entry (synthetic xid).
        let mut storage = super::super::store::Storage::for_tests_null(
            extent(16, 16),
            vk::Format::B8G8R8A8_UNORM,
        );
        // SAFETY: opaque u64 Vk handle for the cursor's view; the
        // stub Storage's `is_test_stub` flag means Drop won't free
        // it. Stamp both views so scene binds the sample-side.
        let cur_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
        storage.image_view = cur_sentinel;
        storage.sample_view = cur_sentinel;
        let cursor_id = store
            .allocate(0xCAFE_0001, DrawableKind::Pixmap, 32, false, storage)
            .expect("alloc cursor stub");

        core.cursor_x = 50.0;
        core.cursor_y = 60.0;
        let cursor = CursorEntry {
            id: cursor_id,
            extent: extent(16, 16),
            hot_x: 0,
            hot_y: 0,
        };

        let scene = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            Some(cursor),
            None,
            None,
        )
        .scene;
        // 1 top-level + 1 cursor = 2.
        assert_eq!(scene.draws.len(), 2);
        let cursor_draw = scene.draws.last().expect("cursor draw");
        assert_eq!(cursor_draw.dst_origin, [50.0, 60.0]);
        assert_eq!(cursor_draw.dst_size, [16.0, 16.0]);
        assert!(
            cursor_draw.alpha_passthrough,
            "cursor must blend (sprite has transparent border)"
        );
    }

    /// Stage 4c.3 / 4c.5 — Automatic-mode invariant.
    ///
    /// When a window W has `redirected_target = Some(B)` AND
    /// `scene_participating == true` (Automatic redirect), the scene
    /// entry for W blits FROM B's storage (its `image_view`), not
    /// from W's own storage. W's geometry (`dst_origin`, `dst_size`)
    /// stays driven by `windows_v2[W]`. `sampled_ids` carries B_id
    /// (not W_id) so damage/fence accounting follows the source the
    /// scene actually read from. B is also marked
    /// `scene_participating=true` per Stage 4c's Automatic-mode
    /// pairing (the protocol handler issues
    /// `set_backing_scene_participation(true)` alongside W's flip).
    ///
    /// 4c.5 rename: framed around the Automatic-mode invariant per
    /// task 4c.5 self-review — the assertion shape already matches.
    #[test]
    fn build_scene_automatic_redirect_keeps_window_via_backing_storage() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // Window W @ (50, 60), 200×100 — emits at output coords
        // (50, 60) since the test output layout origin is (0, 0).
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x100,
            50,
            60,
            200,
            100,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // Allocate a separate backing pixmap B with its OWN sentinel
        // image_view, distinct from W's. B is allocated with
        // `scene_participating=false` (Pixmap default) — that's fine
        // for the build-scene path since the resolution looks up
        // storage directly; only the peek for B's damage needs the
        // flag, which we toggle below to verify the snapshot path
        // keys off `source_id`.
        let mut b_storage = super::super::store::Storage::for_tests_null(
            extent(200, 100),
            vk::Format::B8G8R8A8_UNORM,
        );
        let b_view: vk::ImageView = ash::vk::Handle::from_raw(0xB000_BEEF);
        b_storage.image_view = b_view;
        // Stub both views to the same sentinel — see
        // `alloc_stub_window` for rationale; tests verify
        // routing, not swizzle semantics.
        b_storage.sample_view = b_view;
        let b_id = store
            .allocate(0xB001, DrawableKind::Pixmap, 32, true, b_storage)
            .expect("alloc backing stub");

        // Confirm W and B have distinct image_views.
        let w_id = store.lookup(0x100).expect("w_id present");
        let w_view = store.get(w_id).expect("w drawable").storage.image_view;
        assert_ne!(
            w_view, b_view,
            "fixture sanity: W and B must have distinct sentinel views"
        );

        // Fixture sanity (4c.5 Automatic-mode invariant): W stays
        // scene_participating=true under Automatic redirect; the
        // backing also flips to scene_participating=true (the
        // protocol-side pairing). `alloc_stub_window(mapped=true)`
        // and the `allocate(..., true, _)` above wire both flags.
        assert!(
            store.get(w_id).unwrap().scene_participating,
            "Automatic redirect: W must stay scene_participating=true",
        );
        assert!(
            store.get(b_id).unwrap().scene_participating,
            "Automatic redirect: B must be scene_participating=true",
        );

        // Wire the redirect route: W's source-storage now resolves
        // through B.
        store.set_redirected_target(w_id, Some(b_id));

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            None,
            None,
            None,
        );
        let scene = &built.scene;
        assert_eq!(
            scene.draws.len(),
            1,
            "expected one draw entry for W (geometry unchanged by redirect)"
        );
        let w_draw = &scene.draws[0];

        // Geometry still W's.
        assert_eq!(
            w_draw.dst_origin,
            [50.0, 60.0],
            "redirected W's on-screen rect must remain W's geometry"
        );
        assert_eq!(
            w_draw.dst_size,
            [200.0, 100.0],
            "redirected W's on-screen size must remain W's geometry"
        );

        // Storage handle reroutes to B. The stub fixture stamps
        // both `image_view` and `sample_view` to the same sentinel,
        // so this also implicitly verifies the scene-α fix is
        // binding the sample-side view (no separate handle to
        // distinguish in the stub world — production builds them
        // distinct via `PlatformBackend::build_sample_view`).
        assert_eq!(
            w_draw.image_view, b_view,
            "redirected W must sample FROM B's view, not W's"
        );

        // `sampled_ids` parallels `draws`; the entry for W must
        // carry B_id (the source the scene actually read from) so
        // damage/fence accounting follows the right drawable.
        assert_eq!(built.sampled_ids.len(), 1);
        assert_eq!(
            built.sampled_ids[0], b_id,
            "sampled_ids must carry source_id (B_id) for damage / fence keying"
        );
    }

    /// Stage 4c.5 — Manual-mode invariant.
    ///
    /// `build_scene`'s `scene_participating` filter (scene.rs:1110 and
    /// :922) drops any drawable with `scene_participating == false`
    /// from the per-output draw list. Manual-redirected windows carry
    /// `scene_participating=false` (the protocol handler issues
    /// `set_window_scene_participation(W, false)` on Manual activation)
    /// so they MUST NOT appear in `scene.draws` nor in
    /// `built.sampled_ids`. Plain unredirected/Automatic windows
    /// stay participating and continue to emit.
    ///
    /// Setup: two top-level windows W1 + W2, both mapped and same
    /// geometry shape (so the filter is the only thing distinguishing
    /// them). W1 stays `scene_participating=true`; W2 is flipped to
    /// `false` post-allocation via `set_scene_participating` to
    /// mimic the Manual-redirect activation path. The build must
    /// emit one draw (W1) and zero entries for W2.
    #[test]
    fn build_scene_skips_manual_redirected_window() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // W1 @ (10, 20), 50×40 — Automatic / unredirected
        // (scene_participating=true via `alloc_stub_window`'s
        // `mapped` arg, which the helper forwards as the
        // `scene_participating` flag in `store.allocate`).
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x111,
            10,
            20,
            50,
            40,
            None,
            true,
        );
        core.top_level_order.push(0x111);

        // W2 @ (100, 200), 60×30 — geometry that doesn't overlap
        // W1 so a stray draw entry would be unambiguous.
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x222,
            100,
            200,
            60,
            30,
            None,
            true,
        );
        core.top_level_order.push(0x222);

        // Flip W2 off the scene (Manual-redirect activation). Use
        // the store's setter directly — the backend method does
        // more bookkeeping (damage clear + scene-structure damage
        // rect) than this no-Vk scene-walk test needs.
        let w2_id = store.lookup(0x222).expect("w2 lookup");
        store.set_scene_participating(w2_id, false);
        let w1_id = store.lookup(0x111).expect("w1 lookup");
        assert!(
            store.get(w1_id).unwrap().scene_participating,
            "fixture sanity: W1 stays scene_participating=true",
        );
        assert!(
            !store.get(w2_id).unwrap().scene_participating,
            "fixture sanity: W2 must be scene_participating=false",
        );

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            None,
            None,
            None,
        );
        let scene = &built.scene;

        // Only W1's draw entry must be present.
        assert_eq!(
            scene.draws.len(),
            1,
            "Manual-redirected W2 must be filtered from scene.draws (saw {} entries: {:?})",
            scene.draws.len(),
            scene.draws,
        );
        let w1_draw = &scene.draws[0];
        assert_eq!(
            w1_draw.dst_origin,
            [10.0, 20.0],
            "the surviving draw must be W1 (origin (10,20)), NOT W2 (origin (100,200))",
        );
        assert_eq!(
            w1_draw.dst_size,
            [50.0, 40.0],
            "the surviving draw must be W1 (50×40), NOT W2 (60×30)",
        );

        // sampled_ids mirrors draws — must carry W1's id only.
        assert_eq!(built.sampled_ids.len(), 1);
        assert_eq!(
            built.sampled_ids[0], w1_id,
            "sampled_ids must reference W1; W2 was filtered before push",
        );
    }

    /// Stage 4d — `build_scene` must prune the entire subtree of
    /// a Manual-redirected (`scene_participating=false`) ancestor,
    /// NOT just skip the ancestor node and emit its children.
    ///
    /// Regression context (Stage 4d marco-with-compositing CC
    /// disappears): emit_window_subtree's per-node skip-gate
    /// returned after the SKIP trace but unconditionally recursed
    /// into children. CC's marco-decorated frame (Manual redirect,
    /// scene_participating=false) skipped emit; CC's child GtkWindow
    /// (scene_participating=true, parent=frame) was then walked and
    /// directly emitted by scene_walk — bypassing marco's
    /// compositor entirely. The directly-emitted child storage
    /// (stale, since post-redirect child paints route to the
    /// frame's backing via resolve_paint_target) muddied COW
    /// over marco's compositor output. X11 Composite semantics:
    /// a Manual-redirected window is a *subtree ownership boundary*
    /// — the compositor owns presentation of every descendant.
    ///
    /// Assumption: `scene_participating=false` is set ONLY by
    /// (un)map and `set_window_scene_participation` (Manual-redirect
    /// activation). Unmapped windows are pruned earlier in
    /// emit_window_subtree via `geom.mapped` check, so any
    /// non-participating MAPPED window reaching the per-node gate
    /// is Manual-redirected. If a third reason for
    /// `scene_participating=false` is added later, audit whether it
    /// also wants subtree-prune semantics; if not, the gate must
    /// learn the *reason* (store currently tracks only the bool).
    #[test]
    fn build_scene_prunes_descendants_of_manual_redirected_ancestor() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // Frame W @ (100, 200), 200×150 — the manually-redirected
        // ancestor (CC's marco-decorated frame in production).
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x111,
            100,
            200,
            200,
            150,
            None,
            true,
        );
        core.top_level_order.push(0x111);

        // Child C inside frame W at relative (11, 41), 100×80.
        // scene_participating=true (regular window — only the
        // ancestor is redirected). This is CC's GtkWindow in
        // production: a regular window whose paints route to the
        // frame's redirected backing via resolve_paint_target's
        // ancestor walk.
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x112,
            11,
            41,
            100,
            80,
            Some(0x111),
            true,
        );

        // Bystander top-level W @ (500, 500) so a "did anything
        // get emitted?" assertion isn't ambiguous.
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x222,
            500,
            500,
            60,
            30,
            None,
            true,
        );
        core.top_level_order.push(0x222);

        // Flip frame W to scene_participating=false (Manual
        // redirect activation). Child stays participating.
        let w_frame_id = store.lookup(0x111).expect("frame lookup");
        store.set_scene_participating(w_frame_id, false);
        let child_id = store.lookup(0x112).expect("child lookup");
        assert!(
            store.get(child_id).unwrap().scene_participating,
            "fixture sanity: child stays scene_participating=true",
        );

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            None,
            None,
            None,
        );
        let scene = &built.scene;

        // Only the bystander W (0x222 @ (500,500)) must be drawn.
        // Frame W skipped (Manual redirect) → child C must NOT
        // leak in as a top-level descendant emission.
        assert_eq!(
            scene.draws.len(),
            1,
            "expected exactly 1 draw (the bystander); got {} — children of \
             a Manual-redirected ancestor must be pruned, not walked: {:?}",
            scene.draws.len(),
            scene.draws,
        );
        assert_eq!(
            scene.draws[0].dst_origin,
            [500.0, 500.0],
            "the surviving draw must be the bystander at (500,500); \
             frame and its child were ostensibly pruned",
        );
        // sampled_ids mirrors draws — must not reference child.
        let bystander_id = store.lookup(0x222).expect("bystander lookup");
        assert_eq!(built.sampled_ids, vec![bystander_id]);
    }

    /// Stage 4d — `build_scene` appends the Composite Overlay
    /// Window draw entry ABOVE all top-levels but BELOW the
    /// cursor (Stage 4 plan §4d scene layering items 3 + 4).
    /// Synthetic topology: two top-levels + a registered COW;
    /// the COW entry MUST be the last non-cursor draw in
    /// `scene.draws`. `alpha_passthrough=true` so the
    /// compositor's premul-alpha output blends correctly.
    #[test]
    fn build_scene_appends_cow_above_top_levels() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // Two mapped top-levels — both must appear before the COW.
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x100,
            0,
            0,
            100,
            80,
            None,
            true,
        );
        core.top_level_order.push(0x100);
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x101,
            200,
            150,
            120,
            90,
            None,
            true,
        );
        core.top_level_order.push(0x101);

        // Allocate a stub COW storage (synthetic xid + non-zero
        // sentinel image_view so build_scene doesn't filter it on
        // the null-view gate). Screen-extent (800×600 matches
        // PlatformBackend::for_tests()'s output size).
        let mut cow_storage = super::super::store::Storage::for_tests_null(
            extent(800, 600),
            vk::Format::B8G8R8A8_UNORM,
        );
        let cow_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xC0_C0_C0_C0);
        cow_storage.image_view = cow_sentinel;
        cow_storage.sample_view = cow_sentinel;
        let cow_id = store
            .allocate(0x103, DrawableKind::Window, 24, true, cow_storage)
            .expect("alloc COW stub");

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            None, // no cursor in this fixture
            None,
            Some(cow_id),
        );
        let scene = &built.scene;

        // Expect: top-level 0x100, top-level 0x101, COW. Three
        // entries total (no cursor in this fixture).
        assert_eq!(
            scene.draws.len(),
            3,
            "expected 2 top-levels + COW, got {} draws: {:?}",
            scene.draws.len(),
            scene.draws,
        );

        // COW must be the LAST entry (top of z, since cursor is
        // None). dst_size matches the COW storage extent
        // (800×600). alpha_passthrough must be true.
        let cow_draw = scene.draws.last().expect("COW draw present");
        assert_eq!(
            cow_draw.dst_size,
            [800.0, 600.0],
            "COW draw must carry the screen-extent storage size",
        );
        assert!(
            cow_draw.alpha_passthrough,
            "COW must blend (compositors paint premul-alpha output)",
        );
        // Output layout origin is (0, 0) in the test fixture, so
        // dst_origin is (0, 0).
        assert_eq!(
            cow_draw.dst_origin,
            [0.0, 0.0],
            "COW dst_origin must be (0, 0) absolute after output projection",
        );

        // Both top-levels must come BEFORE the COW entry. Confirm
        // by checking the first two draws are window-sized, NOT
        // screen-sized.
        assert_ne!(
            scene.draws[0].dst_size,
            [800.0, 600.0],
            "first draw must be a top-level, not the COW",
        );
        assert_ne!(
            scene.draws[1].dst_size,
            [800.0, 600.0],
            "second draw must be a top-level, not the COW",
        );

        // sampled_ids must carry cow_id last.
        assert_eq!(
            *built.sampled_ids.last().expect("sampled_ids non-empty"),
            cow_id,
            "sampled_ids must reference COW id at the top of z",
        );
    }

    /// Stage 4d — when a cursor IS registered alongside the COW,
    /// the COW must appear BELOW the cursor (Stage 4 plan §4d
    /// layering item 4: COW is below cursor). Layering oracle:
    /// the cursor draw is last; the COW draw is second-to-last.
    #[test]
    fn build_scene_cow_is_below_cursor() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // One mapped top-level so the scene has anchor content.
        alloc_stub_window(
            &mut store,
            &mut windows_v2,
            0x100,
            0,
            0,
            400,
            300,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // COW @ screen extent.
        let mut cow_storage = super::super::store::Storage::for_tests_null(
            extent(800, 600),
            vk::Format::B8G8R8A8_UNORM,
        );
        let cow_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xC0_C0_C0_C0);
        cow_storage.image_view = cow_sentinel;
        cow_storage.sample_view = cow_sentinel;
        let cow_id = store
            .allocate(0x103, DrawableKind::Window, 24, true, cow_storage)
            .expect("alloc COW stub");

        // Cursor sprite.
        let mut cursor_storage = super::super::store::Storage::for_tests_null(
            extent(16, 16),
            vk::Format::B8G8R8A8_UNORM,
        );
        let cur2_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
        cursor_storage.image_view = cur2_sentinel;
        cursor_storage.sample_view = cur2_sentinel;
        let cursor_id = store
            .allocate(0xCAFE_0002, DrawableKind::Pixmap, 32, false, cursor_storage)
            .expect("alloc cursor stub");
        core.cursor_x = 50.0;
        core.cursor_y = 60.0;
        let cursor = CursorEntry {
            id: cursor_id,
            extent: extent(16, 16),
            hot_x: 0,
            hot_y: 0,
        };

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            Some(cursor),
            None,
            Some(cow_id),
        );
        let scene = &built.scene;

        // Expect: top-level, COW, cursor — 3 draws, in that order.
        assert_eq!(
            scene.draws.len(),
            3,
            "expected top-level + COW + cursor = 3 draws, got {}: {:?}",
            scene.draws.len(),
            scene.draws,
        );
        // Last draw = cursor (16×16).
        assert_eq!(
            scene.draws.last().expect("cursor").dst_size,
            [16.0, 16.0],
            "cursor must be the top-of-z draw",
        );
        // Second-to-last = COW (800×600).
        assert_eq!(
            scene.draws[scene.draws.len() - 2].dst_size,
            [800.0, 600.0],
            "COW must be directly below cursor",
        );
    }
}
