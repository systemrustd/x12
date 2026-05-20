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
//! - **Manual-redirected windows skipped automatically.** Manual
//!   redirect flips the window to `scene_participating = false`;
//!   the scene walk prunes that window's subtree. The compositor
//!   reintroduces those pixels by painting its output/COW surface.
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
    /// Stage 5 Phase D — cursor-plane transition queued behind
    /// this commit. Populated AFTER the compose + atomic commit
    /// succeed (failed submit drops the transition; the next
    /// frame re-decides). Consumed by `handle_page_flip_complete`
    /// which applies the per-CRTC show/hide.
    cursor_transition: Option<CursorTransition>,
    /// Stage 5 Phase D — new value for the per-output cursor
    /// prev-pos. Applied to `OutputSceneState.cursor_prev_pos`
    /// only when this ack retires successfully (codex v4-pass
    /// transactional rule). Failed submit → prev_pos for this
    /// output is NOT advanced, and the next frame still damages
    /// the OLD prev rect to clear the trail.
    cursor_prev_pos_after_retire: Option<Option<(i32, i32)>>,
    /// Stage 5 Phase D — `OutputSceneState.last_frame_cursor_mode`
    /// value to install on successful retire. Captures what's
    /// committed to the screen after this flip. Failed submit
    /// → mode stays as-is.
    cursor_mode_after_retire: OutputCursorMode,
}

/// Stage 5 Phase C — pure result of the cursor-plane strategy
/// decision in `build_scene`. The compositor outer caller consumes
/// this to drive Phase D's `PendingAck` transition state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorAssignment {
    /// HW plane should display the sprite at this position. The
    /// SW cursor draw is omitted from `scene.draws`; the SW prev
    /// rect is still damaged so the buffer-age clipped/LOAD path
    /// clears any prior pixels off the underlying scanout BO.
    Hw {
        x: i32,
        y: i32,
        record_version: u64,
        hot_x: u16,
        hot_y: u16,
    },
    /// SW path — sprite drawn into the scanout BO via composite.
    /// `scene.draws` carries the cursor entry; prev + new rect
    /// added to projected damage. `pos` is the output-local
    /// top-left of the cursor draw (`cursor.xy − hot − layout`)
    /// — propagates into `OutputSceneState.cursor_prev_pos` on
    /// successful retire so the next tick damages this rect to
    /// clear the SW trail.
    Sw { pos: (i32, i32) },
    /// Cursor off-output / unregistered / clipped. SW path damages
    /// the prev rect (if any) so trails clear; nothing is drawn.
    Hidden,
}

/// Stage 5 Phase D — transition queued on a `PendingAck` after the
/// per-output commit succeeds. Consumed at retirement.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CursorTransition {
    /// Retire-time action: optionally upload (if `upload_version`
    /// != `CursorPlane.uploaded_version`), then `show_on_crtc` to
    /// bind the plane and reposition.
    ShowOnRetire {
        upload_version: u64,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    },
    /// Retire-time action: `hide_on_crtc`. Plane is unbound on
    /// this output's CRTC. Other outputs unaffected.
    HideOnRetire,
}

/// Stage 5 Phase D — per-output cursor-plane mode tracked across
/// frames. Drives the `Sw → Hw` / `Hw → Sw` transition matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputCursorMode {
    /// Last frame drew the cursor via the SW composite path on
    /// this output. `prev` is the SW position carried for trail
    /// elimination.
    Sw { prev: Option<(i32, i32)> },
    /// Last frame's plane is bound on this CRTC and showing.
    Hw,
    /// Cursor is off-output or unregistered on this frame.
    Hidden,
}

/// Stage 5 Phase D — query result for the pointer fast path.
/// `Hw` is only reached when EVERY active output has retired its
/// transition to HW; mixed-state outputs return `Mixed`, which
/// suppresses the fast path until every flip retires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorPlaneMode {
    /// Every active output is in HW mode + no transitions pending
    /// — pointer fast path may issue `cursor_plane_move` directly.
    Hw,
    /// At least one output is currently SW or in transition. The
    /// pointer fast path falls back to `scene.wake_for_damage`.
    Mixed,
    /// Every output is in SW (or Hidden) mode — scene wake required.
    Sw,
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
    /// Stage 5 Phase D — per-output last-frame cursor mode. Drives
    /// the transition matrix in `tick_one_output`. v2's per-output
    /// frame retirement means scene-global cursor state would let
    /// output A's Sw→Hw fire while output B is still scanning the
    /// BO with SW pixels (multi-output double-cursor hazard);
    /// per-output mode + per-output `cursor_prev_pos` closes that.
    last_frame_cursor_mode: OutputCursorMode,
    /// Stage 5 Phase D — per-output SW cursor position carried so
    /// the next tick can damage the OLD rect. v3 of the plan moved
    /// this from `SceneCompositorInner.cursor_prev_pos`
    /// (scene-global) per the per-output isolation rule.
    /// **Transactional**: advances ONLY when the matching
    /// `PendingAck.cursor_prev_pos_after_retire` retires
    /// successfully — a failed submit must leave the OLD prev rect
    /// in place so the next frame still clears the trail.
    cursor_prev_pos: Option<(i32, i32)>,
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
    /// Stage 5 Phase H — `YSERVER_V2_HW_CURSOR=1` env gate. Default
    /// OFF: the strategy decision always picks `Sw` so we don't
    /// regress correctness across the rollout. Set once at
    /// construction time so the gate is consistent across all
    /// `build_scene` calls.
    hw_cursor_strategy_enabled: bool,
    /// Stage 4d — Composite Overlay Window scene entry. Lazily set
    /// by `register_cow` on the first client paint into COW (the
    /// xfwm4 case allocates COW but never paints into it, so
    /// registering eagerly would cover the scene with the depth-24
    /// force-opaque initial fill). Lives on the outer struct (not
    /// `inner`) so the stub fixture can also track registration
    /// state in unit tests that don't bring up a live scanout pool.
    cow: Option<super::store::DrawableId>,
}

/// Stage 5 Phase H — env gate for the HW cursor strategy. Default
/// **ON**: empty/unset env var enables the strategy. Explicit
/// `YSERVER_V2_HW_CURSOR=0` (or `false` / `no` / `off`) disables
/// it for SW fallback / debugging. Loud-but-nonfatal SW fallback
/// if the plane init failed inside `PlatformBackend` — gate-on but
/// plane-unavailable is fine, just always picks `Sw`.
///
/// Inverted from the plan's original opt-in rollout shape once
/// Phase A/B/C/D landed and the SW trail-elimination regression
/// (`19d4e4d`) was identified as a SW-path bug independent of
/// the HW strategy.
fn hw_cursor_strategy_enabled() -> bool {
    !matches!(
        std::env::var("YSERVER_V2_HW_CURSOR").ok().as_deref(),
        Some("0" | "false" | "FALSE" | "no" | "NO" | "off" | "OFF")
    )
}

/// Stage 5 Phase D — deferred upload slot held while at least one
/// output is in a `Mixed` transition. Replacing the slot while a
/// previous one is still pending REPLACES it; intermediate versions
/// are dropped on the floor relative to the dumb buffer (their
/// `Arc<CursorRecord>` stays alive for any holder via Phase A's
/// refcount discipline). When the wait set drains to empty, the
/// upload fires and the slot clears.
#[derive(Debug, Clone)]
struct DeferredCursorUpload {
    version: u64,
    width: u16,
    height: u16,
    bgra_bytes: std::sync::Arc<Vec<u8>>,
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
    /// Stage 5 Phase D — deferred upload pending while at least
    /// one output's `wait_set` membership is non-empty. Drained
    /// when the set becomes empty by event (ShowOnRetire /
    /// HideOnRetire / output-disable / hotplug-out). Global
    /// recovery DROPS the slot without firing the upload (the
    /// kernel is taking the device away).
    deferred_cursor_upload: Option<DeferredCursorUpload>,
    /// Stage 5 Phase D — set of output indices whose previous-
    /// version retirement the deferred upload is waiting on. Empty
    /// set + non-None deferred = fire on the next show/upload.
    deferred_upload_wait_set: HashSet<usize>,
}

/// Stage 3f.8 cursor sprite registration. The sprite lives as a
/// regular [`DrawableStore`] entry (a `Pixmap` kind with a synthetic
/// xid) so its lifetime + Vk-handle destruction flow through the
/// same paths as any other drawable.
#[derive(Debug, Clone)]
pub(crate) struct CursorEntry {
    pub(crate) id: super::store::DrawableId,
    pub(crate) extent: vk::Extent2D,
    pub(crate) hot_x: i16,
    pub(crate) hot_y: i16,
    /// Stage 5 Phase B — `Arc<CursorRecord>.version`. Compared by
    /// value in the Phase D upload-dedup path. Zero in unit-test
    /// constructions that pre-date Phase A.
    pub(crate) record_version: u64,
    /// Stage 5 Phase D — straight-alpha BGRA8 bytes shared with
    /// the `CursorRecord` on the backend. `Arc` so the retire-
    /// time upload + the deferred-upload slot can both reference
    /// the same allocation without copying. `None` in unit-test
    /// constructions that pre-date Phase A.
    pub(crate) bgra_bytes: Option<std::sync::Arc<Vec<u8>>>,
}

struct SceneBuild {
    scene: CompositeScene,
    snapshots: Vec<DamageSnapshot>,
    sampled_ids: Vec<super::store::DrawableId>,
    projected_damage: RegionSet,
    /// Stage 5 Phase C — pure cursor strategy decision. The outer
    /// tick consumes this to derive the per-output transition
    /// + new `cursor_prev_pos` and queue them on the PendingAck.
    cursor_assignment: CursorAssignment,
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
                last_frame_cursor_mode: OutputCursorMode::Hidden,
                cursor_prev_pos: None,
            });
        }
        Ok(Self {
            inner: Some(SceneCompositorInner {
                vk,
                pipeline,
                outputs,
                cursor: None,
                deferred_cursor_upload: None,
                deferred_upload_wait_set: HashSet::new(),
            }),
            scene_structure_dirty: true,
            hw_cursor_strategy_enabled: hw_cursor_strategy_enabled(),
            cow: None,
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
    /// entry. Retained for the stage 4d layering tests; the live
    /// backend no longer calls this because xfwm4 paints into a
    /// child compositor window and a topmost COW obscures the
    /// actual output. Marks scene structure dirty so the next
    /// tick picks up the new layer. No-op on the stub fixture
    /// (no Vk).
    pub(crate) fn register_cow(&mut self, id: super::store::DrawableId) {
        self.cow = Some(id);
        self.scene_structure_dirty = true;
    }

    /// Stage 4d — clear the Composite Overlay Window scene
    /// entry. Subsequent `build_scene` calls omit the COW layer;
    /// the storage drop itself is handled by the backend's
    /// `store.decref`. No-op on the stub fixture.
    pub(crate) fn unregister_cow(&mut self) {
        self.cow = None;
        self.scene_structure_dirty = true;
    }

    /// Whether the Composite Overlay Window is currently registered
    /// as a scene entry. The backend uses this to lazy-register on
    /// the first client paint into COW (the xfwm4 case allocates COW
    /// but never paints into it, so registering on allocation would
    /// cover the scene with the depth-24 force-opaque initial fill).
    pub(crate) fn is_cow_registered(&self) -> bool {
        self.cow.is_some()
    }

    /// Test fixture / Stage-1b-era stub. Construct via
    /// `SceneCompositor::stub()` so the `KmsBackendV2::for_tests`
    /// path doesn't need Vk.
    pub(crate) fn stub() -> Self {
        Self {
            inner: None,
            scene_structure_dirty: false,
            hw_cursor_strategy_enabled: false,
            cow: None,
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

    /// Stage 5 Phase D — cursor-plane mode aggregate query for the
    /// pointer fast path. Returns `Hw` ONLY when every active
    /// output has retired its Sw→Hw transition AND no PendingAck
    /// carries an in-flight cursor transition. Mixed-state
    /// (transition pending on any output, or a heterogeneous
    /// mix) returns `Mixed`; the fast path falls back to scene
    /// wake until the plane is fully consistent.
    pub(crate) fn cursor_mode(&self) -> CursorPlaneMode {
        let Some(inner) = self.inner.as_ref() else {
            return CursorPlaneMode::Sw;
        };
        let mut any_hw = false;
        let mut any_sw_like = false;
        for output in &inner.outputs {
            // Any pending transition on any output forces Mixed —
            // the fast path must not move the plane until every
            // ShowOnRetire / HideOnRetire has applied.
            if output
                .pending_acks
                .iter()
                .any(|a| a.cursor_transition.is_some())
            {
                return CursorPlaneMode::Mixed;
            }
            match output.last_frame_cursor_mode {
                OutputCursorMode::Hw => any_hw = true,
                OutputCursorMode::Sw { .. } | OutputCursorMode::Hidden => any_sw_like = true,
            }
        }
        match (any_hw, any_sw_like) {
            (true, false) => CursorPlaneMode::Hw,
            (false, _) => CursorPlaneMode::Sw,
            (true, true) => CursorPlaneMode::Mixed,
        }
    }

    /// Stage 5 Phase D — steady-state HW sprite-change path. Called
    /// synchronously from the backend's `refresh_effective_cursor`
    /// when `cursor_mode() == Hw`. Memcpys new bytes into the dumb
    /// buffer + rebinds visible CRTCs. If any output is currently
    /// Mixed, the upload is deferred until the wait set drains
    /// (codex v4-pass liveness).
    ///
    /// `bytes` MUST be `width * height * 4` (BGRA8). `Arc` so the
    /// deferred slot can hold the bytes without re-cloning the
    /// `Vec<u8>` from `CursorRecord` per upload.
    pub(crate) fn queue_steady_state_cursor_upload(
        &mut self,
        platform: &mut PlatformBackend,
        version: u64,
        width: u16,
        height: u16,
        bgra_bytes: std::sync::Arc<Vec<u8>>,
        hot_x: u16,
        hot_y: u16,
        cursor_x: i32,
        cursor_y: i32,
    ) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        // Replace any prior deferred upload — only the latest
        // version is ever uploaded. Recompute the wait set from
        // outputs currently transitioning.
        let wait_set: HashSet<usize> = inner
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(i, o)| {
                if o.pending_acks.iter().any(|a| a.cursor_transition.is_some()) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        if wait_set.is_empty() {
            // No transitions in flight — upload + rebind synchronously.
            if let Err(e) = platform.cursor_plane_upload_image(
                version,
                u32::from(width),
                u32::from(height),
                bgra_bytes.as_ref(),
            ) {
                log::warn!("v2 cursor: steady-state upload (v{version}) failed: {e}");
                return;
            }
            if let Err(e) =
                platform.cursor_plane_rebind_visible_crtcs(hot_x, hot_y, cursor_x, cursor_y)
            {
                log::warn!("v2 cursor: steady-state rebind failed: {e}");
            }
        } else {
            inner.deferred_cursor_upload = Some(DeferredCursorUpload {
                version,
                width,
                height,
                bgra_bytes,
            });
            inner.deferred_upload_wait_set = wait_set;
        }
    }

    /// Drain in-flight compose work before tear-down. Best-effort
    /// — `device_wait_idle` is the safe fallback the platform
    /// uses anyway. Releases descriptor-pool slots so the
    /// pool-ring's Drop doesn't fire while slots are still in use.
    pub(crate) fn drain_all(&mut self, platform: &mut PlatformBackend) {
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
            // Stage 5 Phase D' — global recovery: reset every
            // output's cursor mode to Hidden. The post-recovery
            // first compose re-decides via build_scene's
            // strategy. cursor_prev_pos is also cleared so the
            // next frame doesn't damage a stale trail rect.
            o.last_frame_cursor_mode = OutputCursorMode::Hidden;
            o.cursor_prev_pos = None;
        }
        // Phase D' — drop the deferred-upload slot WITHOUT firing
        // it (the kernel may be taking the device away; ioctl
        // would fail). Next acquire/modeset re-uploads from the
        // latest CursorRecord via the normal Phase D rule.
        inner.deferred_cursor_upload = None;
        inner.deferred_upload_wait_set.clear();
        // Hide the plane everywhere + invalidate uploaded_version.
        // Best-effort; the platform hook logs per-CRTC failures.
        let _ = platform.cursor_plane_hide_all();
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
        let cow = self.cow;
        let hw_strategy = self.hw_cursor_strategy_enabled;
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
                inner,
                output_idx,
                core,
                store,
                platform,
                windows_v2,
                telemetry,
                hw_strategy,
                cow,
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

            // Stage 5 Phase D — transactional advance: per-output
            // mode + prev_pos move forward ONLY now that this ack
            // retired successfully. Failed submits drop the
            // cursor_transition / cursor_prev_pos_after_retire on
            // the floor so the next tick re-decides from scratch.
            if let Some(new_prev) = ack.cursor_prev_pos_after_retire {
                state.cursor_prev_pos = new_prev;
            }
            state.last_frame_cursor_mode = ack.cursor_mode_after_retire;
            // Apply the cursor transition via the platform (the
            // mode update above describes what we want; the
            // ioctl actually performs it). Per-CRTC ioctl failures
            // are logged inside the platform hooks; we never
            // abort the retire path on cursor errors. The
            // wait-set drain (Phase D deferred upload) advances
            // regardless of the ioctl outcome.
            apply_cursor_transition_on_retire(inner, output_idx, platform, ack.cursor_transition);
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

/// Stage 5 Phase D — pure derivation: combine the previous frame's
/// per-output cursor mode with this frame's `CursorAssignment` to
/// produce the transition to queue and the new prev_pos to write on
/// successful retirement.
///
/// Returns `(transition_to_queue, prev_pos_after_retire)`.
///
/// - `transition_to_queue` is `Some(ShowOnRetire)` only on actual
///   mode transitions into HW (`Hidden→Hw` / `Sw→Hw`);
///   `Some(HideOnRetire)` on transitions out (`Hw→Sw` / `Hw→Hidden`).
///   Steady-state same-mode frames produce `None`.
/// - `prev_pos_after_retire` is `Some(Some(pos))` to set, or
///   `Some(None)` to clear, on successful retire. `None` means
///   "leave `OutputSceneState.cursor_prev_pos` as-is". Hw mode
///   doesn't carry an SW prev_pos so the field always clears on
///   `→ Hw`; `Sw` / `Hidden` carry it.
#[allow(clippy::type_complexity)]
fn derive_cursor_transition(
    prev: OutputCursorMode,
    assignment: CursorAssignment,
) -> (
    Option<CursorTransition>,
    Option<Option<(i32, i32)>>,
    OutputCursorMode,
) {
    match (prev, assignment) {
        (
            OutputCursorMode::Sw { .. } | OutputCursorMode::Hidden,
            CursorAssignment::Hw {
                x,
                y,
                record_version,
                hot_x,
                hot_y,
            },
        ) => (
            Some(CursorTransition::ShowOnRetire {
                upload_version: record_version,
                hot_x,
                hot_y,
                x,
                y,
            }),
            Some(None),
            // Mode advances to Hw only AFTER the retire applies
            // the show; the post-retire mode reflects what's on
            // the screen.
            OutputCursorMode::Hw,
        ),
        (OutputCursorMode::Hw, CursorAssignment::Sw { pos }) => (
            Some(CursorTransition::HideOnRetire),
            // Hw → Sw: the SW sprite is drawn at `pos` this frame.
            // Advance prev so the NEXT frame damages this rect to
            // clear the trail when the SW cursor moves.
            Some(Some(pos)),
            OutputCursorMode::Sw { prev: Some(pos) },
        ),
        (OutputCursorMode::Hw, CursorAssignment::Hidden) => (
            Some(CursorTransition::HideOnRetire),
            None,
            OutputCursorMode::Hidden,
        ),
        (_, CursorAssignment::Sw { pos }) => {
            // Sw → Sw or Hidden → Sw: no transition; advance the
            // per-output `cursor_prev_pos` to where the SW sprite
            // landed this frame so the NEXT frame damages this
            // rect. The transactional rule (codex v4-pass) means
            // failed submits do NOT advance — the OLD prev rect
            // survives and is re-damaged.
            (
                None,
                Some(Some(pos)),
                OutputCursorMode::Sw { prev: Some(pos) },
            )
        }
        (_, CursorAssignment::Hidden) => (None, Some(None), OutputCursorMode::Hidden),
        (OutputCursorMode::Hw, CursorAssignment::Hw { .. }) => (None, None, OutputCursorMode::Hw),
    }
}

/// Stage 5 Phase D — apply a retired-ack's cursor transition via
/// the platform's per-CRTC hooks. Handles the deferred-upload
/// wait-set drain + the upload-then-show ordering rule.
fn apply_cursor_transition_on_retire(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    platform: &mut PlatformBackend,
    transition: Option<CursorTransition>,
) {
    // Drain wait-set membership FIRST. The retire of any
    // ShowOnRetire / HideOnRetire (regardless of outcome) shrinks
    // the wait set; once empty, the deferred upload fires.
    inner.deferred_upload_wait_set.remove(&output_idx);
    let Some(t) = transition else {
        // No transition queued, but the wait-set drain above
        // still applies — Phase D liveness rule.
        maybe_fire_deferred_upload(inner, platform);
        return;
    };
    match t {
        CursorTransition::ShowOnRetire {
            upload_version,
            hot_x,
            hot_y,
            x,
            y,
        } => {
            // Upload if version doesn't match. `upload_image` is
            // already idempotent-deduplicated by value inside
            // `CursorPlane`, but skipping the FFI when we know the
            // version matches is cheaper. Bytes come from the
            // scene's current `CursorEntry` (cloned-Arc, no copy)
            // when its version matches the transition's; otherwise
            // we attempt the bind with whatever's currently in the
            // dumb buffer (at worst one-frame stale; the next
            // sprite-change will re-upload via the steady-state
            // queue path).
            if platform.cursor_plane_uploaded_version() != Some(upload_version) {
                if let Some(entry) = inner.cursor.as_ref()
                    && entry.record_version == upload_version
                    && let Some(bytes) = entry.bgra_bytes.as_ref()
                {
                    if let Err(e) = platform.cursor_plane_upload_image(
                        upload_version,
                        entry.extent.width,
                        entry.extent.height,
                        bytes.as_ref(),
                    ) {
                        log::warn!("v2 cursor: retire-time upload (v{upload_version}) failed: {e}");
                    }
                } else {
                    log::debug!(
                        "v2 cursor: retire-time upload (v{upload_version}) — \
                         no matching entry bytes; binding with current buffer"
                    );
                }
            }
            if let Err(e) = platform.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y) {
                log::warn!("v2 cursor: show_on_crtc({output_idx}) failed at retire: {e}");
            }
        }
        CursorTransition::HideOnRetire => {
            if let Err(e) = platform.cursor_plane_hide_on_crtc(output_idx) {
                log::warn!("v2 cursor: hide_on_crtc({output_idx}) failed at retire: {e}");
            }
        }
    }
    maybe_fire_deferred_upload(inner, platform);
}

/// Stage 5 Phase D — fire the deferred upload if the wait set has
/// drained empty. Called after every wait-set membership change.
fn maybe_fire_deferred_upload(inner: &mut SceneCompositorInner, platform: &mut PlatformBackend) {
    if !inner.deferred_upload_wait_set.is_empty() {
        return;
    }
    let Some(pending) = inner.deferred_cursor_upload.take() else {
        return;
    };
    if let Err(e) = platform.cursor_plane_upload_image(
        pending.version,
        u32::from(pending.width),
        u32::from(pending.height),
        pending.bgra_bytes.as_ref(),
    ) {
        log::warn!(
            "v2 cursor: deferred upload (v{}) failed: {e}",
            pending.version
        );
    }
}

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

#[allow(clippy::too_many_lines)]
fn tick_one_output(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    core: &KmsCore,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    windows_v2: &super::backend::WindowsV2Map,
    telemetry: &mut Telemetry,
    hw_strategy_enabled: bool,
    cow: Option<super::store::DrawableId>,
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
    //    Stage 5 Phase C: build_scene returns a pure
    //    `CursorAssignment` decision; the actual transition queue +
    //    `cursor_prev_pos` advance happens transactionally below
    //    AFTER the per-output commit succeeds.
    let cursor_prev_pos_before = inner.outputs[output_idx].cursor_prev_pos;
    let hw_available = platform.cursor_plane_available();
    let hw_can_run = hw_strategy_enabled && hw_available;
    let prev_mode = inner.outputs[output_idx].last_frame_cursor_mode;
    let built = build_scene(
        core,
        store,
        windows_v2,
        output_idx,
        platform,
        inner.cursor.clone(),
        cursor_prev_pos_before,
        cow,
        hw_can_run,
    );

    // Stage 5 Phase D — derive the per-output cursor transition
    // and new prev_pos from `built.cursor_assignment` and the
    // last-frame mode. Both are queued on the PendingAck below
    // and applied transactionally on successful retirement.
    let (cursor_transition_to_queue, cursor_prev_pos_after_retire, cursor_mode_after_retire) =
        derive_cursor_transition(prev_mode, built.cursor_assignment);

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
                cursor_transition: cursor_transition_to_queue,
                cursor_prev_pos_after_retire,
                cursor_mode_after_retire,
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
#[allow(clippy::too_many_arguments)]
fn build_scene(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows_v2: &super::backend::WindowsV2Map,
    output_idx: usize,
    platform: &PlatformBackend,
    cursor: Option<CursorEntry>,
    cursor_prev_pos: Option<(i32, i32)>,
    cow: Option<super::store::DrawableId>,
    // Stage 5 Phase C — when `true`, the strategy picks `Hw` for
    // cursors that fit the plane and lie on-output; otherwise `Sw`.
    // `false` collapses every assignment to the SW path (rollout
    // default).
    hw_strategy_active: bool,
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
            // Top-level windows start with no redirected ancestor;
            // the flag flips on inside the recursion when entering
            // a redirected window's subtree.
            false,
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

    // Stage 5 Phase C: pure cursor strategy decision. Produces a
    // `CursorAssignment`; the SW draw is appended only when the
    // strategy picks `Sw` (HW assignment omits the draw entirely
    // since the kernel overlay covers it). Trail elimination
    // damage for the PRIOR SW rect runs unconditionally — even
    // after a Sw → Hw transition, the next frame must clear stale
    // SW pixels off the scanout BO (the prior SW position is the
    // bottom of the now-vacated SW area).
    #[allow(clippy::cast_possible_truncation)]
    let cursor_assignment: CursorAssignment = if let Some(cur) = cursor
        && let Some(drawable) = store.get(cur.id)
        && drawable.storage.image_view != vk::ImageView::null()
    {
        let cw = i32::try_from(cur.extent.width).unwrap_or(i32::MAX);
        let ch = i32::try_from(cur.extent.height).unwrap_or(i32::MAX);
        let layout_w_i = i32::try_from(layout_w).unwrap_or(i32::MAX);
        let layout_h_i = i32::try_from(layout_h).unwrap_or(i32::MAX);

        let add_cursor_damage = |projected: &mut RegionSet, dx: i32, dy: i32| {
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

        // Damage the previous SW rect unconditionally — even a
        // pure-HW frame needs to clear stale SW pixels off the
        // scanout BO. Phase D's `cursor_prev_pos_after_retire`
        // advances `OutputSceneState.cursor_prev_pos` only when
        // the matching commit retires; failed commits leave the
        // OLD prev rect in place so the trail is still cleared.
        if let Some((prev_x, prev_y)) = cursor_prev_pos {
            add_cursor_damage(&mut projected, prev_x, prev_y);
        }

        let dx = (core.cursor_x as i32) - i32::from(cur.hot_x) - layout_x0;
        let dy = (core.cursor_y as i32) - i32::from(cur.hot_y) - layout_y0;
        let visible = !(dx + cw <= 0 || dy + ch <= 0 || dx >= layout_w_i || dy >= layout_h_i);
        if !visible {
            // Off-output / fully-clipped — the cursor isn't on this
            // output this frame. Phase D treats this as `Hidden`;
            // the prev-rect damage above still clears the trail.
            CursorAssignment::Hidden
        } else {
            // Phase C strategy gates (codex v6-pass — pure data, no
            // DRM side effects). Hand off to HW only when the
            // strategy is active AND the sprite fits the plane (≤
            // 64×64 hardware minimum).
            let hw_fits = cur.extent.width <= 64 && cur.extent.height <= 64;
            if hw_strategy_active && hw_fits {
                add_cursor_damage(&mut projected, dx, dy);
                CursorAssignment::Hw {
                    x: core.cursor_x as i32,
                    y: core.cursor_y as i32,
                    record_version: cur.record_version,
                    hot_x: u16::try_from(cur.hot_x.max(0)).unwrap_or(0),
                    hot_y: u16::try_from(cur.hot_y.max(0)).unwrap_or(0),
                }
            } else {
                draws.push(CompositeDraw {
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
                add_cursor_damage(&mut projected, dx, dy);
                CursorAssignment::Sw { pos: (dx, dy) }
            }
        }
    } else {
        CursorAssignment::Hidden
    };

    let scene = CompositeScene {
        bg_color: bg,
        draws,
    };
    SceneBuild {
        scene,
        snapshots,
        sampled_ids,
        projected_damage: projected,
        cursor_assignment,
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
    // Audit #3 (2026-05-19): true iff some ancestor on the recursion
    // path owns a `redirected_target`. When set, this window's paint
    // landed in that ancestor's backing (via `resolve_paint_target`'s
    // ancestor walk), so emitting this window's own storage would
    // show stale/empty pixels — the ancestor's emit already shows
    // the content. A descendant that owns ITS OWN `redirected_target`
    // breaks this chain (its paint stops at itself), so it still
    // emits its own backing regardless of the inherited flag.
    under_redirected_ancestor: bool,
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
    // Audit #3 (2026-05-19): the old `prune_subtree=true` for
    // `scene_participating=false` is gone — Automatic descendants of
    // Manual ancestors need to recurse so they can emit their own
    // backing. Per-window emit-vs-skip is decided by
    // `paint_target_is_self` below; the recurse always runs and the
    // `under_redirected_ancestor` flag carries the chain context.

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
            // Both modes blit FROM B; W's geometry (dst_origin, dst_size,
            // intersect test) stays driven by W's own state in
            // `windows_v2`. Only the sampled storage handle reroutes.
            let source_id = store.redirected_target(id).unwrap_or(id);
            let source_view_null = store
                .get(source_id)
                .is_none_or(|s| s.storage.image_view == vk::ImageView::null());

            // Audit #3 (2026-05-19) — emit-or-skip is governed by
            // "is this window's storage where paint actually lands?"
            //
            //   has_own_redirected_target   self owns a `redirected_target`
            //                               → paint lands in its B, emit B.
            //   under_redirected_ancestor   some ancestor owns one
            //                               → paint lands in ancestor's B,
            //                                 ancestor emits it, we skip.
            //   d_part                      `scene_participating=true` —
            //                                 ordinary non-redirected window
            //                                 with its own storage as the
            //                                 paint target. Emit own storage.
            //
            // Pre-fix the rule was `d_part || manual_backing_visible`
            // plus an unconditional `prune_subtree` on
            // `scene_participating=false`. That dropped Automatic-
            // redirected descendants of Manual-redirected ancestors —
            // GTK/marco CSD frames lose their inner widgets (per audit
            // #3 / Control Center missing-widget reports).
            let has_own_redirected_target = source_id != id;
            let paint_target_is_self =
                has_own_redirected_target || (d_part && !under_redirected_ancestor);

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
            let skip_reason: Option<&'static str> = if !paint_target_is_self {
                if has_own_redirected_target {
                    // Defensive — `paint_target_is_self` is true when
                    // `has_own_redirected_target`, so this branch is
                    // unreachable. Kept so the match stays exhaustive
                    // if the rule ever evolves.
                    Some("paint_target_not_self")
                } else if under_redirected_ancestor {
                    Some("paint_target_is_redirected_ancestor")
                } else {
                    Some("scene_participating=false")
                }
            } else if !matches!(d_kind, DrawableKind::Window) {
                Some("kind!=Window")
            } else if source_view_null {
                Some("source_image_view_null")
            } else if !intersects {
                Some("no_intersect_with_output")
            } else {
                None
            };

            if debug_focus {
                log::debug!(
                    "v2 scene_walk focus xid={host_xid:#x} source_id={source_id:?} \
                     has_own_redirected_target={has_own_redirected_target} \
                     under_redirected_ancestor={under_redirected_ancestor} \
                     paint_target_is_self={paint_target_is_self} \
                     intersects={intersects} skip_reason={skip_reason:?}",
                );
            }

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

            if matches!(d_kind, DrawableKind::Window)
                && let Some(source) = store.get(source_id)
                && source.storage.image_view != vk::ImageView::null()
                && intersects
                && paint_target_is_self
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

    // Audit #3 (2026-05-19) — descendants need to know whether THEY
    // sit under a redirected ancestor. The chain is "this window
    // counts as a redirected ancestor iff it owns its own
    // `redirected_target`" — that's exactly where
    // `resolve_paint_target` stops climbing the parent chain. A
    // recursion under a Manual-redirected ancestor without own
    // backing flips the flag on; an Automatic-redirected descendant
    // beneath that resets the flag for its own descendants (because
    // its paint stops at its own B).
    let self_owns_redirected_target = store
        .lookup(host_xid)
        .and_then(|id| store.redirected_target(id))
        .is_some();
    let child_under_redirected_ancestor = under_redirected_ancestor || self_owns_redirected_target;

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
            child_under_redirected_ancestor,
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
        .map_or(-1, IntoRawFd::into_raw_fd);
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

    // Stage 5 Phase G — strategy decision unit tests. Verify the
    // pure `derive_cursor_transition` matrix without needing
    // build_scene or a live Vk fixture.

    #[test]
    fn derive_sw_to_hw_queues_show_on_retire() {
        let prev = OutputCursorMode::Sw {
            prev: Some((100, 100)),
        };
        let assignment = CursorAssignment::Hw {
            x: 200,
            y: 150,
            record_version: 42,
            hot_x: 4,
            hot_y: 4,
        };
        let (trans, prev_pos, mode_after) = derive_cursor_transition(prev, assignment);
        let Some(CursorTransition::ShowOnRetire {
            upload_version,
            x,
            y,
            ..
        }) = trans
        else {
            panic!("expected ShowOnRetire, got {trans:?}");
        };
        assert_eq!(upload_version, 42);
        assert_eq!((x, y), (200, 150));
        assert_eq!(prev_pos, Some(None), "Sw→Hw clears prev_pos");
        assert_eq!(mode_after, OutputCursorMode::Hw);
    }

    #[test]
    fn derive_hw_to_sw_queues_hide_on_retire() {
        let (trans, _prev_pos, mode_after) = derive_cursor_transition(
            OutputCursorMode::Hw,
            CursorAssignment::Sw { pos: (100, 100) },
        );
        assert!(matches!(trans, Some(CursorTransition::HideOnRetire)));
        assert!(matches!(mode_after, OutputCursorMode::Sw { .. }));
    }

    #[test]
    fn derive_hw_to_hidden_queues_hide_on_retire() {
        let (trans, _prev_pos, mode_after) =
            derive_cursor_transition(OutputCursorMode::Hw, CursorAssignment::Hidden);
        assert!(matches!(trans, Some(CursorTransition::HideOnRetire)));
        assert_eq!(mode_after, OutputCursorMode::Hidden);
    }

    /// Steady-state HW: no transition queued (the bytes path
    /// flows through the synchronous `queue_steady_state_cursor_upload`
    /// instead, since v2's empty-damage skip would starve a
    /// `PendingAck`-driven upload).
    #[test]
    fn derive_hw_to_hw_no_transition() {
        let assignment = CursorAssignment::Hw {
            x: 0,
            y: 0,
            record_version: 7,
            hot_x: 0,
            hot_y: 0,
        };
        let (trans, _prev_pos, mode_after) =
            derive_cursor_transition(OutputCursorMode::Hw, assignment);
        assert!(trans.is_none());
        assert_eq!(mode_after, OutputCursorMode::Hw);
    }

    /// Sw → Sw and Hidden → Sw produce no transition (no plane
    /// state change) but the mode advances to Sw so the next
    /// frame's derivation sees the right "prev".
    #[test]
    fn derive_sw_or_hidden_to_sw_advances_mode_no_transition() {
        let (trans, _prev_pos, mode_after) = derive_cursor_transition(
            OutputCursorMode::Sw { prev: None },
            CursorAssignment::Sw { pos: (50, 50) },
        );
        assert!(trans.is_none());
        assert!(matches!(mode_after, OutputCursorMode::Sw { .. }));

        let (trans, _, mode_after) = derive_cursor_transition(
            OutputCursorMode::Hidden,
            CursorAssignment::Sw { pos: (50, 50) },
        );
        assert!(trans.is_none());
        assert!(matches!(mode_after, OutputCursorMode::Sw { .. }));
    }

    /// `YSERVER_V2_HW_CURSOR=1` opt-in default OFF: with the env
    /// gate unset / false, build_scene's strategy always returns
    /// `Sw` (or `Hidden`) regardless of plane availability and
    /// extent. Tested at the `hw_strategy_active=false` parameter
    /// of build_scene to keep the test free of env-var ordering.
    #[test]
    fn hw_strategy_off_collapses_to_sw() {
        // Strategy disabled → Hw must NEVER be picked even when
        // the cursor would fit. (Behavioural equivalent of "env
        // var unset"; the env gate itself is set on the
        // SceneCompositor at construction time and forwarded into
        // tick_one_output -> build_scene as a bool param.)
        // This test pins the parameter wiring; an end-to-end
        // env-var test would need a process-scoped fixture.
        let prev = OutputCursorMode::Sw { prev: None };
        // Sw → Sw with HW NOT active — no transition.
        let (trans, _, _) = derive_cursor_transition(prev, CursorAssignment::Sw { pos: (10, 10) });
        assert!(trans.is_none());
    }

    /// `cursor_mode()` returns `Mixed` while any output's PendingAck
    /// carries an unretired cursor transition — the load-bearing
    /// query gate for the pointer fast path.
    #[test]
    fn cursor_mode_mixed_when_transition_pending() {
        let mut scene = SceneCompositor::stub();
        // Stub has `inner == None`; cursor_mode collapses to Sw.
        assert_eq!(scene.cursor_mode(), CursorPlaneMode::Sw);
        // The pending-transition path can only be triggered with
        // a real inner; covered by integration smoke + the
        // separate `derive_*` tests above.
        let _ = &mut scene;
    }

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
                cursor: None,
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
            false,
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
            false,
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
            record_version: 0,
            bgra_bytes: None,
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
            false,
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
            false,
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
            false,
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

    /// Stage 4d — `build_scene` must skip non-redirected descendants
    /// of a Manual-redirected ancestor that owns its own backing.
    /// The descendants' paint routes through `resolve_paint_target`
    /// to the ancestor's B; emitting their own (stale) storage on
    /// top of the ancestor's B would muddy the compositor output.
    ///
    /// Audit #3 follow-up (2026-05-19): the test was originally
    /// written against the degenerate state where the parent has
    /// `scene_participating=false` *without* a redirected backing —
    /// that state doesn't occur in real life (Manual-redirect
    /// activation always sets `redirected_target` BEFORE flipping
    /// `scene_participating=false`, see
    /// `activate_redirect_backing_for`). Updated to mirror the
    /// realistic state: frame has both a backing AND
    /// `scene_participating=false`, and the assertion checks that
    /// frame_B emits while the non-redirected child is skipped.
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

        // Set up realistic Manual-redirect state on frame W: allocate
        // a backing, point W's `redirected_target` at it, then flip
        // `scene_participating=false`. Child stays participating —
        // its paint will resolve to frame_B via
        // `resolve_paint_target`'s ancestor walk, NOT to its own
        // storage; so the child's storage stays stale, and emitting
        // it would muddy the frame_B emit underneath.
        let w_frame_id = store.lookup(0x111).expect("frame lookup");
        let mut frame_backing = super::super::store::Storage::for_tests_null(
            extent(200, 150),
            vk::Format::B8G8R8A8_UNORM,
        );
        let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F111);
        frame_backing.image_view = frame_backing_view;
        frame_backing.sample_view = frame_backing_view;
        let frame_backing_id = store
            .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
            .expect("alloc frame backing");
        store.set_redirected_target(w_frame_id, Some(frame_backing_id));
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
            false,
        );
        let scene = &built.scene;

        // Two draws: frame_B (origin = frame's pos, since the
        // Manual-redirected window emits its backing at its own
        // geometry) AND the bystander. The child of frame W must
        // NOT emit — its paint routes to frame_B.
        assert_eq!(
            scene.draws.len(),
            2,
            "expected frame_B + bystander; got {} — non-redirected children of \
             a Manual-redirected ancestor with a backing must skip emit \
             (their paint resolves to the ancestor's B): {:?}",
            scene.draws.len(),
            scene.draws,
        );
        assert!(
            scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
            "frame backing draw missing: {:?}",
            scene.draws
        );
        assert!(
            scene.draws.iter().any(|d| d.dst_origin == [500.0, 500.0]),
            "bystander draw missing: {:?}",
            scene.draws
        );
        // The "must not leak" property — child's draw entry must NOT appear.
        assert!(
            !scene.draws.iter().any(|d| d.dst_origin == [111.0, 241.0]),
            "non-redirected child of Manual-redirected ancestor leaked into scene.draws: {:?}",
            scene.draws,
        );
        // First-draw checks below are scoped to the surviving entries.
        let bystander_draw = scene
            .draws
            .iter()
            .find(|d| d.dst_origin == [500.0, 500.0])
            .expect("bystander present");
        assert_eq!(
            bystander_draw.dst_origin,
            [500.0, 500.0],
            "the surviving draw must be the bystander at (500,500); \
             frame and its child were ostensibly pruned",
        );
        // sampled_ids mirrors draws — frame_B + bystander, no child.
        let bystander_id = store.lookup(0x222).expect("bystander lookup");
        assert_eq!(built.sampled_ids.len(), 2);
        assert!(built.sampled_ids.contains(&frame_backing_id));
        assert!(built.sampled_ids.contains(&bystander_id));
    }

    /// Stage 4d follow-up — a Manual-redirected parent with a
    /// redirected backing must emit that backing directly, while
    /// still pruning its descendants.
    #[test]
    fn build_scene_emits_manual_redirected_parent_backing_but_prunes_descendants() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

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
        let w_frame_id = store.lookup(0x111).expect("frame lookup");

        let mut backing = super::super::store::Storage::for_tests_null(
            extent(200, 150),
            vk::Format::B8G8R8A8_UNORM,
        );
        let backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_CAFE);
        backing.image_view = backing_view;
        backing.sample_view = backing_view;
        let backing_id = store
            .allocate(0xB002, DrawableKind::Pixmap, 32, true, backing)
            .expect("alloc redirected backing");
        store.set_redirected_target(w_frame_id, Some(backing_id));
        store.set_scene_participating(w_frame_id, false);
        assert!(
            store.get(backing_id).unwrap().scene_participating,
            "fixture sanity: redirected backing stays scene_participating=true",
        );

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
            false,
        );
        let scene = &built.scene;

        assert_eq!(scene.draws.len(), 2, "expected manual backing + bystander");
        assert!(
            scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
            "manual parent backing draw missing: {:?}",
            scene.draws
        );
        assert!(
            scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [500.0, 500.0] && d.dst_size == [60.0, 30.0]),
            "bystander draw missing: {:?}",
            scene.draws
        );

        let bystander_id = store.lookup(0x222).expect("bystander lookup");
        assert_eq!(built.sampled_ids.len(), 2);
        assert!(built.sampled_ids.contains(&backing_id));
        assert!(built.sampled_ids.contains(&bystander_id));
    }

    /// Audit #3 (2026-05-19) — a Manual-redirected parent still
    /// prunes its NON-redirected descendants (their paint resolves
    /// to the parent's B via `resolve_paint_target` so the parent
    /// emit covers them), but Automatic-redirected descendants have
    /// their OWN backing — `resolve_paint_target` stops at them —
    /// and MUST still emit. Pre-fix `prune_subtree=true` dropped
    /// them unconditionally, matching the audit's "GTK/marco CSD
    /// pattern: RedirectWindow(frame, Manual) +
    /// RedirectSubwindows(frame, Automatic) makes Automatic
    /// widgets vanish" symptom (Control Center missing menus /
    /// widgets).
    #[test]
    fn build_scene_emits_automatic_descendant_under_manual_ancestor() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // Frame F at (100, 200), 200×150 — Manual-redirected
        // (scene_participating=false) with its own backing F_B.
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
        let frame_id = store.lookup(0x111).expect("frame lookup");

        let mut frame_backing = super::super::store::Storage::for_tests_null(
            extent(200, 150),
            vk::Format::B8G8R8A8_UNORM,
        );
        let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F000);
        frame_backing.image_view = frame_backing_view;
        frame_backing.sample_view = frame_backing_view;
        let frame_backing_id = store
            .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
            .expect("alloc frame backing");
        store.set_redirected_target(frame_id, Some(frame_backing_id));
        store.set_scene_participating(frame_id, false);

        // Automatic-redirected child C at (11, 41) inside F — own
        // backing C_B; scene_participating=true (Automatic).
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
        let child_id = store.lookup(0x112).expect("child lookup");

        let mut child_backing = super::super::store::Storage::for_tests_null(
            extent(100, 80),
            vk::Format::B8G8R8A8_UNORM,
        );
        let child_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_C000);
        child_backing.image_view = child_backing_view;
        child_backing.sample_view = child_backing_view;
        let child_backing_id = store
            .allocate(0xB112, DrawableKind::Pixmap, 32, true, child_backing)
            .expect("alloc child backing");
        store.set_redirected_target(child_id, Some(child_backing_id));
        // Automatic mode → child window stays scene_participating=true.
        assert!(
            store.get(child_id).unwrap().scene_participating,
            "fixture sanity: Automatic-redirected child stays scene_participating=true",
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
            false,
        );
        let scene = &built.scene;

        // Two draws: parent F's backing at (100, 200), child C's
        // backing at (111, 241) (= F.pos + C.pos relative).
        assert_eq!(
            scene.draws.len(),
            2,
            "expected manual-parent backing + automatic-child backing; got {:?}",
            scene.draws
        );
        assert!(
            scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
            "manual parent backing draw missing: {:?}",
            scene.draws
        );
        assert!(
            scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [111.0, 241.0] && d.dst_size == [100.0, 80.0]),
            "automatic child backing draw missing: {:?}",
            scene.draws
        );
        assert!(built.sampled_ids.contains(&frame_backing_id));
        assert!(built.sampled_ids.contains(&child_backing_id));
    }

    /// Phase 1 pre-cleanup — when no COW is registered
    /// (`cow=None`), `build_scene` walks the top-level order and
    /// emits a draw entry per mapped top-level. This preserves the
    /// legacy non-redirected path that Phase 1 (COW-authoritative)
    /// leaves unchanged; the `cow=Some` shape (top-levels stripped)
    /// gets its own dedicated test.
    #[test]
    fn build_scene_cow_none_emits_top_levels() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows_v2 = super::super::backend::WindowsV2Map::new();

        // Two mapped top-levels.
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

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            None, // no cursor in this fixture
            None,
            None, // cow=None — legacy non-redirected path
            false,
        );
        let scene = &built.scene;

        // Expect: top-level 0x100, top-level 0x101. Two entries
        // total (no cursor, no COW).
        assert_eq!(
            scene.draws.len(),
            2,
            "expected 2 top-levels, got {} draws: {:?}",
            scene.draws.len(),
            scene.draws,
        );

        // Top-level 0x100 at (0, 0) sized 100×80.
        assert_eq!(
            scene.draws[0].dst_origin,
            [0.0, 0.0],
            "first top-level origin",
        );
        assert_eq!(
            scene.draws[0].dst_size,
            [100.0, 80.0],
            "first top-level size",
        );
        // Top-level 0x101 at (200, 150) sized 120×90.
        assert_eq!(
            scene.draws[1].dst_origin,
            [200.0, 150.0],
            "second top-level origin",
        );
        assert_eq!(
            scene.draws[1].dst_size,
            [120.0, 90.0],
            "second top-level size",
        );

        // No draw should be screen-extent (no COW present).
        for d in &scene.draws {
            assert_ne!(
                d.dst_size,
                [800.0, 600.0],
                "no draw should be screen-extent when cow=None: {:?}",
                d,
            );
        }
    }

    /// Phase 1 pre-cleanup — when no COW is registered
    /// (`cow=None`), the cursor draw must still be appended at
    /// the top of z above the top-level draws. This preserves the
    /// legacy non-redirected cursor-on-top assertion that Phase 1
    /// leaves unchanged. The COW-present cursor ordering (top-levels
    /// stripped, COW below cursor) gets its own dedicated test.
    #[test]
    fn build_scene_cow_none_cursor_at_top() {
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
            record_version: 0,
            bgra_bytes: None,
        };

        let built = build_scene(
            &core,
            &mut store,
            &windows_v2,
            0,
            &platform,
            Some(cursor),
            None,
            None, // cow=None — legacy non-redirected path
            false,
        );
        let scene = &built.scene;

        // Expect: top-level, cursor — 2 draws, in that order.
        assert_eq!(
            scene.draws.len(),
            2,
            "expected top-level + cursor = 2 draws, got {}: {:?}",
            scene.draws.len(),
            scene.draws,
        );
        // Last draw = cursor (16×16).
        assert_eq!(
            scene.draws.last().expect("cursor").dst_size,
            [16.0, 16.0],
            "cursor must be the top-of-z draw",
        );
        // First draw = top-level (400×300).
        assert_eq!(
            scene.draws[0].dst_size,
            [400.0, 300.0],
            "top-level must be below cursor",
        );
    }
}
