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
    store::{DamageSnapshot, DrawableKind, DrawableStore},
};
use crate::kms::{
    core::KmsCore,
    scheduler::composite_pool_ring::CompositePoolRing,
    vk::{
        compositor::{CompositeDraw, CompositeScene, PresentError, record_and_present_composite},
        pipeline::{CompositorPipeline, MAX_DESCRIPTOR_SETS_PER_FRAME},
    },
};

// ────────────────────────────────────────────────────────────────
// Per-output state
// ────────────────────────────────────────────────────────────────

/// Per-output pending-ack ledger. Each entry corresponds to one
/// in-flight compose; popped front on page-flip-complete.
struct PendingAck {
    bo_idx: usize,
    /// Snapshots taken at tick entry, one per source drawable
    /// that contributed to the compose. Ack'd against the
    /// store's live presentation damage on flip retirement.
    drawable_snapshots: Vec<DamageSnapshot>,
    /// Engine fence ticket for the source drawables touched by
    /// the compose. Stage 2e wires this into compose-read
    /// consumer tracking; Stage 2d holds it so prior paint
    /// work keeps its inner alive past compose-record time.
    #[allow(dead_code)]
    ticket: Option<FenceTicket>,
}

struct OutputSceneState {
    output_idx: usize,
    pool_ring: CompositePoolRing,
    /// Slots map: pending_ack[i] is using descriptor-pool slot
    /// `pool_slots[i]`. Released to the ring on flip retirement.
    pool_slots: VecDeque<usize>,
    pending_acks: VecDeque<PendingAck>,
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
        for i in 0..platform.outputs.len() {
            let ring = CompositePoolRing::new(Arc::clone(&vk), MAX_DESCRIPTOR_SETS_PER_FRAME)
                .map_err(SceneError::Vk)?;
            outputs.push(OutputSceneState {
                output_idx: i,
                pool_ring: ring,
                pool_slots: VecDeque::with_capacity(4),
                pending_acks: VecDeque::with_capacity(4),
            });
        }
        Ok(Self {
            inner: Some(SceneCompositorInner {
                vk,
                pipeline,
                outputs,
            }),
            scene_structure_dirty: true,
        })
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
    /// tick to fire. Idempotent.
    pub(crate) fn mark_scene_structure_dirty(&mut self) {
        self.scene_structure_dirty = true;
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
        // Pop the matching ack. We assume FIFO: the oldest pending
        // ack matches the just-retired flip. Stage 2e tightens
        // this with explicit (output_idx, generation) keys.
        if let Some(ack) = state.pending_acks.pop_front() {
            for snap in ack.drawable_snapshots {
                store.ack_presentation_damage(snap);
            }
            // Release the matching pool slot.
            if let Some(slot) = state.pool_slots.pop_front() {
                state.pool_ring.release(slot);
            }
            // Record present generation on the BO (Stage 2e
            // wires the buffer-age algorithm against this).
            let g = platform.record_present(output_idx, retire.presented_bo_idx);
            platform.commit_bo_present(output_idx, retire.presented_bo_idx, g);
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
    // 1. Acquire next BO. None means all BOs in flight.
    let token = match platform.acquire_scanout_bo(output_idx) {
        Some(t) => t,
        None => return Ok(false),
    };
    // 2. Build the scene. Bottom-to-top: root → top-level windows
    //    in z-order → cursor (skipped Stage 2d).
    let (scene, snapshots) = build_scene(core, store, windows_v2, output_idx, platform);
    if scene.draws.is_empty() && all_zero(scene.bg_color) {
        // Nothing visible. Skip compose entirely.
        return Ok(false);
    }

    // 3. Acquire a descriptor-pool slot for this frame.
    let state = inner
        .outputs
        .get_mut(output_idx)
        .expect("output_idx in range");
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

    // 4. Record + present. Reuses v1's helper for the heavy
    //    lifting (record CB into bo.vk_transfer.command_buffer +
    //    queue_submit2 with signal_semaphore = bo.vk_semaphore +
    //    export_sync_file + submit_flip_with_fences atomic).
    let pool = platform
        .scanout_pools
        .get_mut(output_idx)
        .and_then(|p| p.as_mut())
        .ok_or(SceneError::NoVk)?;
    let layout = &platform.outputs[output_idx];
    let bo = pool.bos.get_mut(token.bo_idx).ok_or(SceneError::NoVk)?;
    record_and_present_composite(
        &inner.vk,
        &platform.device,
        &layout.output,
        bo,
        &inner.pipeline,
        descriptor_pool,
        &scene,
    )?;

    // 5. Touch render-fence on every source drawable. Stage 2c's
    //    paint ops already touch it for the writer side; this
    //    closes the reader side per cross-cutting §5.
    // Note: scene's draws don't carry the drawable id directly;
    // we use the snapshots' ids (one per drawable that contributed
    // a sampleable view). Stage 2d skips the FenceTicket plumbing
    // for compose-read — it's a Stage 2e concern (load-bearing
    // there for buffer-age correctness across BO rotation). For
    // 2d's full-redraw-every-tick path, the touch_render_fence
    // is correctness-equivalent to skipping it because every tick
    // reads every drawable anyway.
    for snap in &snapshots {
        let _ = snap;
        // Stage 2e: `store.touch_render_fence(snap.id, ticket.clone())`.
    }

    // 6. Park pending ack.
    state.pool_slots.push_back(slot);
    state.pending_acks.push_back(PendingAck {
        bo_idx: token.bo_idx,
        drawable_snapshots: snapshots,
        ticket: None,
    });
    Ok(true)
}

fn all_zero(c: [f32; 4]) -> bool {
    c[0] == 0.0 && c[1] == 0.0 && c[2] == 0.0 && c[3] == 0.0
}

/// Walk window tree, build the per-output scene + collect damage
/// snapshots.
///
/// Stage 2d simplifications:
/// - Skip the root storage entirely — bg_pixel is the clear color
///   (`scene.bg_color`). `bg_pixmap` would need a sample-from-pixmap
///   that uses the same blit pipeline as windows, deferred to
///   Stage 3 alongside the rest of the root content pipeline.
/// - Top-level windows only, no descendants. Z-order is
///   `core.top_level_order` (back-to-front). Subwindow draws land
///   when Stage 3 generalises window-tree compositing.
/// - Cursor skipped (no cursor storage in Stage 2d).
fn build_scene(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows_v2: &super::backend::WindowsV2Map,
    output_idx: usize,
    platform: &PlatformBackend,
) -> (CompositeScene, Vec<DamageSnapshot>) {
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
    for &host_xid in &core.top_level_order {
        let Some(geom) = windows_v2.get(&host_xid) else {
            continue;
        };
        if !geom.mapped {
            continue;
        }
        let Some(id) = store.lookup(host_xid) else {
            continue;
        };
        let Some(drawable) = store.get(id) else {
            continue;
        };
        if !drawable.scene_participating {
            // Manual-redirected backings, unmapped windows.
            continue;
        }
        if !matches!(drawable.kind, DrawableKind::Window) {
            continue;
        }
        let image_view = drawable.storage.image_view;
        if image_view == vk::ImageView::null() {
            continue;
        }
        // Project window geometry into output-local coords.
        let dx = i32::from(geom.x) - layout_x0;
        let dy = i32::from(geom.y) - layout_y0;
        // Trivial reject if the window doesn't intersect the
        // output at all.
        let win_w = i32::from(geom.width);
        let win_h = i32::from(geom.height);
        if dx + win_w <= 0
            || dy + win_h <= 0
            || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
            || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX)
        {
            continue;
        }
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
        // Peek damage. Always peek (Stage 2d full-redraw doesn't
        // use the snapshot for clipping; threading it now lets
        // 2e bolt buffer-age clipping without touching the
        // scene-assembly loop).
        if let Some(snap) = store.peek_presentation_damage(id) {
            snapshots.push(snap);
        }
    }
    let scene = CompositeScene {
        bg_color: bg,
        draws,
    };
    (scene, snapshots)
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
}
