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

use std::{collections::VecDeque, sync::Arc};

use ash::vk;

use super::{
    platform::{FenceTicket, PlatformBackend},
    store::{DamageSnapshot, DrawableKind, DrawableStore, RegionSet},
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
                damage_history: BufferAgeRing::new(bo_depth + 1),
                current_generation: 0,
                scene_structure_damage: RegionSet::new(),
                pending_repaint_after_failed_submit: RegionSet::new(),
                output_extent: vk::Extent2D {
                    width: u32::from(layout.width),
                    height: u32::from(layout.height),
                },
            });
        }
        Ok(Self {
            inner: Some(SceneCompositorInner {
                vk,
                pipeline,
                outputs,
                cursor: None,
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
    /// callable from any mutation path that wants the next
    /// tick to fire. Idempotent. **Coarse**: adds a full-output
    /// rect to every output's scene_structure_damage. Stage 3+
    /// can call [`mark_scene_structure_damage_rect`] for
    /// rectangle-precise tracking.
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
            match tick_one_output(inner, output_idx, core, store, platform, windows_v2) {
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
    ) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(retire) = platform.on_page_flip_complete(output_idx) else {
            return;
        };
        let Some(state) = inner.outputs.get_mut(output_idx) else {
            return;
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
        } else {
            log::debug!(
                "v2 scene: page-flip-complete on output {output_idx} \
                 with no pending ack — startup flush or spurious event",
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Per-output compose tick body
// ────────────────────────────────────────────────────────────────

fn tick_one_output(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    core: &KmsCore,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    windows_v2: &super::backend::WindowsV2Map,
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
        let s = inner.outputs.get(output_idx).expect("range");
        if !s.pending_acks.is_empty() {
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
    let (scene, snapshots, projected_damage) =
        build_scene(core, store, windows_v2, output_idx, platform, inner.cursor);
    let mut output_damage = projected_damage;
    output_damage.union_with(&scene_structure_snap);
    output_damage.union_with(&failed_repaint_snap);

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
    let repaint = if scene.draws.is_empty() {
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
    let pool = platform
        .scanout_pools
        .get_mut(output_idx)
        .and_then(|p| p.as_mut())
        .ok_or(SceneError::NoVk)?;
    let layout = &platform.outputs[output_idx];
    let bo = pool.bos.get_mut(token.bo_idx).ok_or(SceneError::NoVk)?;
    let compose_result = record_compose_v2(
        &inner.vk,
        &platform.device,
        &layout.output,
        bo,
        &inner.pipeline,
        descriptor_pool,
        &scene,
        repaint,
    );

    let state = inner.outputs.get_mut(output_idx).expect("range");
    match compose_result {
        Ok(()) => {
            state.pool_slots.push_back(slot);
            state.pending_acks.push_back(PendingAck {
                bo_idx: token.bo_idx,
                generation: frame_gen,
                drawable_snapshots: snapshots,
                ticket: None,
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
                    // 9b — atomic commit failed after queue
                    // submit succeeded. BO contents indeterminate.
                    platform.invalidate_bo(output_idx, token.bo_idx);
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
            // Both failure paths share the same recovery: release
            // the descriptor-pool slot, fold the repaint into
            // pending_repaint_after_failed_submit, do NOT push a
            // pending_ack, do NOT advance current_generation.
            // Re-borrow `state` after the platform.invalidate_bo
            // call (which took &mut platform).
            let state = inner.outputs.get_mut(output_idx).expect("range");
            state.pool_ring.release(slot);
            if let Some(br) = output_damage.bounding_rect() {
                state.pending_repaint_after_failed_submit.add(br);
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
) -> (CompositeScene, Vec<DamageSnapshot>, RegionSet) {
    let bg = core
        .bg_pixel
        .map(super::engine::decode_x11_pixel_bgra)
        .unwrap_or([0.0, 0.0, 0.0, 1.0]);
    let layout = &platform.outputs[output_idx];
    let layout_x0 = layout.x;
    let layout_y0 = layout.y;
    let layout_w = u32::from(layout.width);
    let layout_h = u32::from(layout.height);

    let mut draws: Vec<CompositeDraw> = Vec::new();
    let mut snapshots: Vec<DamageSnapshot> = Vec::new();
    let mut projected = RegionSet::new();
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
            &mut projected,
        );
    }

    // Stage 3f.8: append the cursor sprite at top of z. Coordinates
    // are output-local: `core.cursor_x` / `core.cursor_y` are
    // root-space, and we subtract the output layout origin (the
    // same projection windows go through above). `alpha_passthrough`
    // is `true` so the sprite's alpha channel actually blends
    // against the underlying composite instead of force-opaque.
    if let Some(cur) = cursor
        && let Some(drawable) = store.get(cur.id)
        && drawable.storage.image_view != vk::ImageView::null()
    {
        let cw = i32::try_from(cur.extent.width).unwrap_or(i32::MAX);
        let ch = i32::try_from(cur.extent.height).unwrap_or(i32::MAX);
        let dx = (core.cursor_x as i32) - i32::from(cur.hot_x) - layout_x0;
        let dy = (core.cursor_y as i32) - i32::from(cur.hot_y) - layout_y0;
        let visible = !(dx + cw <= 0
            || dy + ch <= 0
            || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
            || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));
        if visible {
            draws.push(CompositeDraw {
                image_view: drawable.storage.image_view,
                #[allow(clippy::cast_precision_loss)]
                dst_origin: [dx as f32, dy as f32],
                #[allow(clippy::cast_precision_loss)]
                dst_size: [cw as f32, ch as f32],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: true,
            });
            // Cursor damage so the next tick covers the sprite
            // rect even when no other draws contributed.
            projected.add(vk::Rect2D {
                offset: vk::Offset2D {
                    x: dx.max(0),
                    y: dy.max(0),
                },
                extent: vk::Extent2D {
                    width: u32::try_from(cw).unwrap_or(0),
                    height: u32::try_from(ch).unwrap_or(0),
                },
            });
        }
    }

    let scene = CompositeScene {
        bg_color: bg,
        draws,
    };
    (scene, snapshots, projected)
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
    projected: &mut RegionSet,
) {
    let Some(geom) = windows_v2.get(&host_xid) else {
        return;
    };
    if !geom.mapped {
        // X11: an unmapped window (and entire subtree) is invisible.
        return;
    }
    let abs_x = parent_abs_x + i32::from(geom.x);
    let abs_y = parent_abs_y + i32::from(geom.y);

    // Emit a draw entry for this window if it has live storage that
    // participates in the scene.
    if let Some(id) = store.lookup(host_xid)
        && let Some(drawable) = store.get(id)
        && drawable.scene_participating
        && matches!(drawable.kind, DrawableKind::Window)
        && drawable.storage.image_view != vk::ImageView::null()
    {
        let image_view = drawable.storage.image_view;
        // Project onto output-local coords.
        let dx = abs_x - layout_x0;
        let dy = abs_y - layout_y0;
        let win_w = i32::from(geom.width);
        let win_h = i32::from(geom.height);
        // Trivial reject only if the window doesn't intersect the
        // output at all.
        let intersects = !(dx + win_w <= 0
            || dy + win_h <= 0
            || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
            || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));
        if intersects {
            draws.push(CompositeDraw {
                image_view,
                #[allow(clippy::cast_precision_loss)]
                dst_origin: [dx as f32, dy as f32],
                #[allow(clippy::cast_precision_loss)]
                dst_size: [win_w as f32, win_h as f32],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: false,
            });
            if let Some(snap) = store.peek_presentation_damage(id) {
                for r in snap.region.rects() {
                    projected.add(vk::Rect2D {
                        offset: vk::Offset2D {
                            x: r.offset.x + dx,
                            y: r.offset.y + dy,
                        },
                        extent: r.extent,
                    });
                }
                snapshots.push(snap);
            }
        }
    }

    // Recurse into mapped descendants. Sibling z-order is HashMap
    // iteration order (see fn-level comment) — proper stack tracking
    // is post-3f.6.
    let children: Vec<u32> = windows_v2
        .iter()
        .filter_map(|(xid, g)| {
            if g.parent == Some(host_xid) {
                Some(*xid)
            } else {
                None
            }
        })
        .collect();
    for child_xid in children {
        emit_window_subtree(
            child_xid, abs_x, abs_y, store, windows_v2, layout_x0, layout_y0, layout_w, layout_h,
            draws, snapshots, projected,
        );
    }
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
            .queue_submit2(vk.graphics_queue, &submit, vk::Fence::null())?;
    }

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
            bo.state = crate::kms::vk::scanout::BoState::default();
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
        let windows = super::super::backend::WindowsV2Map::new();
        let err = scene
            .tick(&core, &mut store, &mut platform, &windows)
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
        storage.image_view = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
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

        let (scene, _snaps, _proj) =
            build_scene(&core, &mut store, &windows_v2, 0, &platform, None);
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

        let (scene, _snaps, _proj) =
            build_scene(&core, &mut store, &windows_v2, 0, &platform, None);
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
        // it.
        storage.image_view = ash::vk::Handle::from_raw(0xCAFE_BABE);
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

        let (scene, _snaps, _proj) =
            build_scene(&core, &mut store, &windows_v2, 0, &platform, Some(cursor));
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
}
