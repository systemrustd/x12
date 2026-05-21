//! `PlatformBackend` — hardware + OS surface for the v2 renderer.
//!
//! Per rendering-model-v2 spec § "PlatformBackend — hardware + OS
//! surface" and Stage 2 plan
//! (`docs/superpowers/plans/2026-05-16-stage-2.md`) substage 2a.
//! Owns the DRM device, KMS outputs, libinput context, Vulkan
//! device, command pool, recyclable fence pool, and per-output
//! scanout BO pools (with v2's per-BO generation tracking for
//! the buffer-age algorithm).
//!
//! Exposes the **two-sync-object** API the v2 model needs:
//! [`FenceTicket`] for CPU-side resource lifetime (I6a), and the
//! per-`ScanoutBo` long-lived `vk_semaphore` (consumed by KMS
//! `IN_FENCE_FD`) for the page-flip kernel wait. The
//! `KmsSyncSemaphore` wrapper from the Stage 2 plan turned out
//! to be unnecessary — `ScanoutBoPool` already owns reusable
//! per-BO export semaphores, so v2 reuses those directly.
//! Stage 2a's commit message records this departure.
//!
//! `KmsBackendV2` holds `platform: PlatformBackend` and
//! delegates DRM / Vk / libinput access through it. Paint paths
//! still log gaps in Stage 2a; the real `DrawableStore` /
//! `RenderEngine` / `SceneCompositor` arrive in Stage 2b–2e.
//!
//! Several APIs introduced here (`FenceTicket`, `FencePool`,
//! `ScanoutBoToken`, `PageFlipRetirement`, `invalidate_bo`,
//! `record_present`, `commit_bo_present`) are dead-code in 2a —
//! they're the surface 2b–2e consume. The dead-code allowances
//! below get retired one at a time as later substages land.

#![allow(
    dead_code,
    reason = "FenceTicket / scanout BO primitives are consumed by Stages 2b–2e"
)]

use std::{
    io,
    os::fd::{AsFd, AsRawFd, RawFd},
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use ash::vk;
use yserver_core::backend::BackendFdKind;

use crate::{
    drm,
    kms::{
        backend::{OutputLayout, PlatformInit, platform_init as core_platform_init},
        v2::store::Storage,
        vk::{
            device::VkContext,
            ops::OpsCommandPool,
            scanout::{BoPhase, BoState, ScanoutBoPool},
        },
    },
};

// ────────────────────────────────────────────────────────────────
// FenceTicket — CPU-side I6a lifetime ticket.
//
// One `FenceTicket` per submission, cloneable across consumers.
// Wraps an `Rc<FenceTicketInner>` so the underlying `vk::Fence`
// survives until every consumer drops its clone. On the final
// drop, if the fence has been observed signaled, it's recycled
// back to the platform's pool; otherwise it leaks (and a
// renderer_failed flag is set), since recycling an unsignaled
// fence whose GPU work might still reference resources would
// be a use-after-free.
//
// Per Stage 2 plan cross-cutting §1.
// ────────────────────────────────────────────────────────────────

/// A submission's CPU-side lifetime ticket. Cloneable; each
/// clone holds a refcount on the inner. The underlying
/// `vk::Fence` is returned to the platform's pool on the
/// final-drop iff it has been observed signaled.
///
/// `Arc<FenceTicketInner>` (rather than `Rc`) keeps the type
/// `Send`, which the `Backend` trait requires (KmsBackendV2:
/// Backend; Backend: Send). The single-threaded core invariant
/// means there's no real cross-thread access; the `Arc` is
/// paying a trivial atomic for type-system uniformity.
#[derive(Clone)]
pub(crate) struct FenceTicket {
    inner: Arc<FenceTicketInner>,
}

struct FenceTicketInner {
    fence: vk::Fence,
    /// Set on the first `poll_signaled` that observes
    /// `vk::SUCCESS`. After this, `poll_signaled` short-circuits
    /// without calling the driver. `AtomicBool` avoids a Mutex
    /// for this hot field.
    signaled_cache: AtomicBool,
    /// Weak handle to the platform's fence pool. On `Drop`, if
    /// the fence is signaled AND the pool still exists, return
    /// the fence handle to the pool. If not signaled, leak the
    /// fence handle and set `renderer_failed` on the platform.
    pool: Weak<Mutex<FencePoolInner>>,
}

impl FenceTicket {
    /// Non-blocking signaled check. Caches `true` once observed
    /// so subsequent calls don't hit the driver.
    pub(crate) fn poll_signaled(&self, vk: &VkContext) -> bool {
        if self.inner.signaled_cache.load(Ordering::Acquire) {
            return true;
        }
        // ash's `get_fence_status` returns `Result<bool, vk::Result>`
        // where the bool is the signaled state (Ok(true) =
        // VK_SUCCESS, Ok(false) = VK_NOT_READY). Errors are real
        // driver failures.
        match unsafe { vk.device.get_fence_status(self.inner.fence) } {
            Ok(true) => {
                self.inner.signaled_cache.store(true, Ordering::Release);
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::warn!("FenceTicket::poll_signaled: get_fence_status: {e:?}");
                false
            }
        }
    }

    /// Synchronous wait. **Off the hot path** — used by
    /// `get_image` readback and shutdown teardown.
    pub(crate) fn wait(&self, vk: &VkContext) -> Result<(), vk::Result> {
        if self.inner.signaled_cache.load(Ordering::Acquire) {
            return Ok(());
        }
        // 5 second timeout — long enough to cover any realistic
        // GPU work; if we hit it the device is hung anyway.
        match unsafe {
            vk.device
                .wait_for_fences(&[self.inner.fence], true, 5_000_000_000)
        } {
            Ok(()) => {
                self.inner.signaled_cache.store(true, Ordering::Release);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Raw fence handle for `vkQueueSubmit2`. Caller MUST NOT
    /// destroy or reset this fence — the ticket owns its
    /// lifetime via the pool.
    pub(crate) fn fence(&self) -> vk::Fence {
        self.inner.fence
    }
}

impl Drop for FenceTicketInner {
    fn drop(&mut self) {
        let Some(pool) = self.pool.upgrade() else {
            // Pool already gone (backend teardown finished
            // before this clone dropped). The fence handle's
            // destruction follows VkContext drop, which already
            // tore down the device — nothing to do.
            return;
        };
        let Ok(mut pool) = pool.lock() else {
            log::error!("FenceTicketInner::drop: fence-pool mutex poisoned");
            return;
        };
        let signaled = self.signaled_cache.load(Ordering::Acquire)
            || match unsafe { pool.vk.device.get_fence_status(self.fence) } {
                Ok(true) => {
                    self.signaled_cache.store(true, Ordering::Release);
                    true
                }
                Ok(false) => false,
                Err(e) => {
                    log::warn!("FenceTicketInner::drop: get_fence_status: {e:?}");
                    false
                }
            };
        if signaled {
            pool.recycle(self.fence);
        } else {
            // Unsignaled drop: per the spec, recycling here
            // would race the still-pending GPU work that names
            // this fence (it might be referenced by an
            // in-flight submit). Leak the handle and flag the
            // renderer as failed so the next op surfaces the
            // condition.
            log::error!(
                "FenceTicket: leaked unsignaled fence {:?} on drop \
                 — renderer_failed will be set on next platform access",
                self.fence,
            );
            pool.renderer_failed = true;
            pool.leaked_fences.push(self.fence);
        }
    }
}

// ────────────────────────────────────────────────────────────────
// FencePool — recyclable VkFence allocator.
//
// Simple stack: `acquire` either pops a recycled (already-reset)
// fence or creates a new one; `recycle` pushes back after
// resetting the fence. `Drop` walks the entire pool (including
// leaked unsignaled handles) and destroys each fence.
// ────────────────────────────────────────────────────────────────

pub(crate) struct FencePool {
    inner: Arc<Mutex<FencePoolInner>>,
}

struct FencePoolInner {
    vk: Arc<VkContext>,
    /// Free list of fences known to be in the unsignaled
    /// (reset) state, ready to be passed to `vkQueueSubmit2`.
    free: Vec<vk::Fence>,
    /// Handles deliberately leaked because they were dropped
    /// while still potentially in flight. Destroyed only at
    /// `Drop` after `vkDeviceWaitIdle`.
    leaked_fences: Vec<vk::Fence>,
    /// Set when `FenceTicketInner::Drop` observes an unsignaled
    /// fence — the renderer is no longer safe to continue.
    renderer_failed: bool,
}

impl FencePoolInner {
    fn recycle(&mut self, fence: vk::Fence) {
        // Reset to unsignaled so the next acquire can re-pass
        // the handle straight to vkQueueSubmit2 (which requires
        // unsignaled).
        if let Err(e) = unsafe { self.vk.device.reset_fences(&[fence]) } {
            log::warn!("FencePool::recycle: reset_fences: {e:?} — leaking fence");
            self.leaked_fences.push(fence);
            return;
        }
        self.free.push(fence);
    }
}

impl FencePool {
    pub(crate) fn new(vk: Arc<VkContext>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FencePoolInner {
                vk,
                free: Vec::with_capacity(8),
                leaked_fences: Vec::new(),
                renderer_failed: false,
            })),
        }
    }

    fn acquire(&self) -> Result<FenceTicket, vk::Result> {
        let mut pool = self
            .inner
            .lock()
            .map_err(|_| vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let fence = if let Some(f) = pool.free.pop() {
            f
        } else {
            let info = vk::FenceCreateInfo::default();
            unsafe { pool.vk.device.create_fence(&info, None)? }
        };
        drop(pool);
        Ok(FenceTicket {
            inner: Arc::new(FenceTicketInner {
                fence,
                signaled_cache: AtomicBool::new(false),
                pool: Arc::downgrade(&self.inner),
            }),
        })
    }

    pub(crate) fn renderer_failed(&self) -> bool {
        self.inner.lock().map(|p| p.renderer_failed).unwrap_or(true)
    }
}

impl Drop for FencePool {
    fn drop(&mut self) {
        let Ok(pool) = self.inner.lock() else {
            return;
        };
        // Best-effort wait so any still-in-flight fence
        // (shouldn't happen but be defensive) is safe to
        // destroy.
        unsafe {
            let _ = pool.vk.device.device_wait_idle();
            for &f in &pool.free {
                pool.vk.device.destroy_fence(f, None);
            }
            for &f in &pool.leaked_fences {
                pool.vk.device.destroy_fence(f, None);
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────
// BoGenerationEntry / ScanoutBoToken / PageFlipRetirement —
// I6b retirement signal infra augmenting ScanoutBoPool's BoState.
// ────────────────────────────────────────────────────────────────

/// Per-BO v2 augmentation parallel to `ScanoutBo::state` (which
/// tracks the Vk/KMS sync state machine). This carries the
/// buffer-age algorithm's `last_present_generation` and the
/// failed-flip `content_invalidated` flag.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BoGenerationEntry {
    /// Last successful page-flip's generation on this BO.
    /// `None` means freshly-allocated (never presented) OR
    /// invalidated (see `content_invalidated`).
    pub(crate) last_present_generation: Option<u64>,
    /// `true` after a failed atomic commit where this BO's
    /// contents became indeterminate. Cleared on next
    /// successful present.
    pub(crate) content_invalidated: bool,
}

/// Handle returned by `acquire_scanout_bo`. Carries the
/// information the SceneCompositor needs to drive the
/// buffer-age algorithm without poking at `ScanoutBoPool`
/// internals.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanoutBoToken {
    pub(crate) output_idx: usize,
    pub(crate) bo_idx: usize,
    pub(crate) extent: vk::Extent2D,
    pub(crate) last_present_generation: Option<u64>,
    pub(crate) content_invalidated: bool,
}

/// Returned by `on_page_flip_complete`. Identifies the BO that
/// just retired (releasable for reuse on next acquire) and the
/// BO that just went on-screen (caller advances its
/// `last_present_generation`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageFlipRetirement {
    pub(crate) retired_bo_idx: Option<usize>,
    pub(crate) presented_bo_idx: usize,
    pub(crate) generation: u64,
}

// ────────────────────────────────────────────────────────────────
// PlatformBackend
// ────────────────────────────────────────────────────────────────

/// v2's real DRM/Vk/libinput owner. Replaces the flat field set
/// that Stage 1b's `KmsBackendV2` carried.
pub(crate) struct PlatformBackend {
    // DRM / output side
    pub(crate) device: Arc<drm::Device>,
    pub(crate) render_node_fd: Option<std::os::fd::OwnedFd>,
    pub(crate) render_node_path: Option<PathBuf>,
    pub(crate) outputs: Vec<OutputLayout>,
    pub(crate) fb_w: u16,
    pub(crate) fb_h: u16,

    // Input side
    input_ctx: Option<crate::input::SendContext>,

    // Vulkan side. `Option` only to support test fixtures that
    // skip Vk init (`for_tests`). Production `open_with_commit`
    // always returns `Some`. v2 has no pixman fallback.
    pub(crate) vk: Option<Arc<VkContext>>,
    /// Wrapped in `Option` for the same reason. Drop order
    /// matters: ops_command_pool BEFORE fence_pool BEFORE vk
    /// (handled by struct field order — Rust drops fields in
    /// declaration order).
    pub(crate) ops_command_pool: Option<OpsCommandPool>,
    pub(crate) fence_pool: Option<FencePool>,

    /// Stage 3f.10: recycled `(image, view, memory)` triples for
    /// CreatePixmap. Reuses v1's `PixmapPool` verbatim — its
    /// `try_take` / `try_return` API + bucket-cap + size-cap
    /// logic is backend-agnostic. Bypassed by the test fixture
    /// (`for_tests`) and on `for_tests_with_vk` (the harness
    /// constructs `RenderEngine` directly without going through
    /// `open_with_commit`).
    pub(crate) pixmap_pool: Option<Arc<crate::kms::vk::pixmap_pool::PixmapPool>>,

    /// Per-output scanout BO pool. `None` if a particular
    /// output's allocation failed (rare; e.g. RADV/gfx8 quirks).
    /// Stage 2c+ paint paths skip output indices with `None`
    /// pool, mirroring v1's behaviour.
    pub(crate) scanout_pools: Vec<Option<ScanoutBoPool>>,

    /// Per-output, per-BO generation entries. `bo_generations[oi][bi]`
    /// pairs with `scanout_pools[oi].as_ref().unwrap().bos[bi]`.
    /// `Vec::new()` for outputs whose pool is `None`.
    pub(crate) bo_generations: Vec<Vec<BoGenerationEntry>>,
    /// Monotonic per-platform counter. Each successful present
    /// gets a fresh generation; SceneCompositor's `frame_gen`
    /// derives from `current_generation + 1` per spec.
    pub(crate) next_present_generation: u64,

    /// Per-output flag — was the first pageflip-complete event
    /// logged for this output? Mirrors v1's `first_pageflip_logged`.
    pub(crate) first_pageflip_logged: Vec<bool>,

    /// Latched on any submit-time / pool-time Vk error. Once
    /// true, the renderer is in a stuck state and the next
    /// composite tick should bail.
    pub(crate) renderer_failed: bool,
    pub(crate) shutting_down: bool,

    /// Stage 5 Phase B — DRM hardware cursor plane. `None` if init
    /// failed (best-effort; SW fallback kicks in) or on the test
    /// fixture. The shared dumb buffer + per-CRTC visibility map
    /// live inside `CursorPlane` itself.
    pub(crate) cursor_plane: Option<crate::kms::cursor_plane::CursorPlane>,
}

impl PlatformBackend {
    /// Production constructor. Opens DRM, initialises Vk,
    /// allocates per-output scanout pools, builds the fence
    /// pool. All-or-nothing: any failure tears down already-
    /// allocated resources and returns Err.
    ///
    /// # Errors
    ///
    /// Propagates platform-init failures from `core_platform_init`,
    /// Vk init failures from `VkContext::new`, command-pool
    /// allocation failures from `OpsCommandPool::new`. ScanoutBoPool
    /// failures per-output are non-fatal — that output is marked
    /// `None` and skipped.
    pub(crate) fn open_with_commit(
        device_path: &str,
        commit: fn(
            &drm::Device,
            &drm::modeset::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        let PlatformInit {
            device,
            render_node_fd,
            render_node_path,
            layouts,
            fb_w,
            fb_h,
            input_ctx,
        } = core_platform_init(device_path, commit)?;

        let vk = match VkContext::new() {
            Ok(v) => v,
            Err(e) => {
                return Err(io::Error::other(format!(
                    "v2 PlatformBackend: VkContext init failed (v2 requires Vulkan; \
                     no pixman fallback): {e:?}"
                )));
            }
        };
        log::info!(
            "v2 PlatformBackend: VkContext ready (driver_id={:?})",
            vk.driver_id,
        );

        let ops_command_pool = OpsCommandPool::new(Arc::clone(&vk))
            .map_err(|e| io::Error::other(format!("ops command pool: {e:?}")))?;

        let fence_pool = FencePool::new(Arc::clone(&vk));

        // Stage 3f.10: pixmap pool reuses v1's allocator verbatim.
        // MATE / xfce4 / GTK widgets churn ~90 pixmap allocs/sec;
        // without this every CreatePixmap pays a full
        // create_image + allocate_memory + bind + create_view cycle.
        // Registers with the GLOBAL_LATEST_POOL hook so the main-
        // loop telemetry path can sample hit/miss counters even
        // though v2 doesn't own the telemetry-emit cadence directly.
        let pixmap_pool = {
            let p = Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(
                &vk,
            )));
            crate::kms::vk::pixmap_pool::register_for_telemetry(&p);
            Some(p)
        };

        // One ScanoutBoPool per output, 3-BO depth (matches v1).
        let mut scanout_pools = Vec::with_capacity(layouts.len());
        let mut bo_generations = Vec::with_capacity(layouts.len());
        for (i, layout) in layouts.iter().enumerate() {
            let w = u32::from(layout.width);
            let h = u32::from(layout.height);
            match ScanoutBoPool::allocate(
                Arc::clone(&vk),
                Arc::clone(&device),
                w,
                h,
                3,
                &layout.output.scanout_modifiers,
            ) {
                Ok(pool) => {
                    let n = pool.bos.len();
                    scanout_pools.push(Some(pool));
                    bo_generations.push(vec![BoGenerationEntry::default(); n]);
                }
                Err(e) => {
                    log::warn!(
                        "v2: ScanoutBoPool allocate failed for output {i} ({}x{}): {e:?} \
                         — output will be skipped from compose",
                        w,
                        h,
                    );
                    scanout_pools.push(None);
                    bo_generations.push(Vec::new());
                }
            }
        }
        let first_pageflip_logged = vec![false; layouts.len()];

        // Stage 5 Phase B — bring up the DRM cursor plane. Failure
        // is non-fatal; v2 falls back to the SW scene cursor path.
        let crtc_handles: Vec<::drm::control::crtc::Handle> =
            layouts.iter().map(|l| l.output.crtc).collect();
        let cursor_plane =
            match crate::kms::cursor_plane::CursorPlane::new(Arc::clone(&device), &crtc_handles) {
                Ok(plane) => {
                    log::info!(
                        "v2 PlatformBackend: hardware cursor plane initialised (64x64 ARGB8888)"
                    );
                    Some(plane)
                }
                Err(e) => {
                    log::warn!(
                        "v2 PlatformBackend: cursor plane init failed ({e}); SW cursor fallback",
                    );
                    None
                }
            };

        log::info!(
            "v2 PlatformBackend: ready — {} outputs, fb {}x{}, {} scanout pools live",
            layouts.len(),
            fb_w,
            fb_h,
            scanout_pools.iter().filter(|p| p.is_some()).count(),
        );

        Ok(Self {
            device,
            render_node_fd,
            render_node_path,
            outputs: layouts,
            fb_w,
            fb_h,
            input_ctx,
            vk: Some(vk),
            ops_command_pool: Some(ops_command_pool),
            fence_pool: Some(fence_pool),
            pixmap_pool,
            scanout_pools,
            bo_generations,
            next_present_generation: 0,
            first_pageflip_logged,
            renderer_failed: false,
            shutting_down: false,
            cursor_plane,
        })
    }

    /// Headless test seed. No DRM device, no Vk, single
    /// stub 800×600 output. Mirrors `KmsBackendV2::for_tests`'s
    /// existing shape from Stage 1b.
    #[doc(hidden)]
    pub(crate) fn for_tests() -> Self {
        Self {
            device: Arc::new(drm::Device::for_tests().expect("test drm device")),
            render_node_fd: None,
            render_node_path: None,
            outputs: vec![OutputLayout {
                output: drm::modeset::Output {
                    connector: ::drm::control::from_u32(1).unwrap(),
                    connector_name: "test".to_string(),
                    crtc: ::drm::control::from_u32(1).unwrap(),
                    plane: ::drm::control::from_u32(1).unwrap(),
                    // SAFETY: tests never pass this mode to DRM.
                    mode: unsafe { std::mem::zeroed() },
                    picked: drm::modeset::Mode {
                        name: "test".to_string(),
                        width: 800,
                        height: 600,
                        vrefresh: 60,
                        preferred: true,
                    },
                    plane_fb_id_prop: ::drm::control::from_u32(1).unwrap(),
                    plane_crtc_id_prop: ::drm::control::from_u32(1).unwrap(),
                    scanout_modifiers: Vec::new(),
                },
                swapchain: drm::Swapchain::empty_for_tests(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                damage: crate::kms::scheduler::damage::OutputDamageState::new(),
                composite_pools: None,
            }],
            fb_w: 800,
            fb_h: 600,
            input_ctx: None,
            vk: None,
            ops_command_pool: None,
            fence_pool: None,
            pixmap_pool: None,
            scanout_pools: vec![None],
            bo_generations: vec![Vec::new()],
            next_present_generation: 0,
            first_pageflip_logged: vec![false],
            renderer_failed: false,
            shutting_down: false,
            cursor_plane: None,
        }
    }

    pub(crate) fn fb_dimensions(&self) -> (u16, u16) {
        (self.fb_w, self.fb_h)
    }

    // ── Stage 5 Phase B — hardware cursor-plane hooks ─────────────
    //
    // The plan splits the legacy `set_cursor2`-driven path into
    // narrow per-CRTC primitives so the Phase D `PendingAck`
    // transition state machine can drive the plane without
    // re-introducing the multi-output double-cursor hazard.
    //
    // - `cursor_plane_available()` is consulted by `build_scene`'s
    //   pure `CursorAssignment` decision.
    // - `cursor_plane_upload_image` memcpys bytes into the shared
    //   dumb buffer ONLY. It does NOT call `set_cursor2`.
    //   `set_cursor2(Some, …)` IS the show operation in legacy DRM;
    //   upload-as-show would prematurely bind on CRTCs whose Sw→Hw
    //   transition hasn't retired yet.
    // - `cursor_plane_show_on_crtc` is the sole `set_cursor2(Some,
    //   …)` site, called per-output from `handle_page_flip_complete`
    //   when that CRTC's PendingAck queues a `ShowOnRetire`. The
    //   immediate `move_to` follow-up is required because some
    //   kernels reset the cursor position to (0, 0) on rebind (v1
    //   pattern at `backend.rs:2173`).
    // - `cursor_plane_rebind_visible_crtcs` is the steady-state
    //   sprite-swap path: rebind only on CRTCs ALREADY showing the
    //   cursor; the rebind-then-move pair runs synchronously off
    //   the protocol handler thread.
    // - `cursor_plane_move` is the pointer-fast-path entry point;
    //   one ioctl per visible CRTC, no GPU work.
    // - `cursor_plane_hide_on_crtc` and `cursor_plane_hide_all`
    //   serve Phase D' output-local / global recovery respectively.

    /// True iff the cursor plane was successfully initialised at
    /// boot. The scene strategy decision (`CursorAssignment`) gates
    /// on this without holding a `PlatformBackend` borrow.
    #[must_use]
    pub(crate) fn cursor_plane_available(&self) -> bool {
        self.cursor_plane.is_some()
    }

    /// Memcpy `bgra_bytes` into the shared dumb buffer iff
    /// `version` differs from the plane's tracked
    /// `uploaded_version`. **No `set_cursor2`**. Idempotent on
    /// repeated calls with the same version.
    ///
    /// # Errors
    /// `InvalidInput` for dims > 64×64 or short byte slice; ioctl
    /// errors are not returned by `load_image`.
    pub(crate) fn cursor_plane_upload_image(
        &mut self,
        version: u64,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()> {
        let Some(plane) = self.cursor_plane.as_mut() else {
            return Err(io::Error::other("cursor plane unavailable"));
        };
        plane.upload_image(version, width, height, bgra_bytes)
    }

    /// Version currently held in the dumb buffer. Compared by VALUE
    /// in the Phase B/C upload-dedup paths.
    #[must_use]
    pub(crate) fn cursor_plane_uploaded_version(&self) -> Option<u64> {
        self.cursor_plane
            .as_ref()
            .and_then(|p| p.uploaded_version())
    }

    /// Bind the plane on `output_idx`'s CRTC + position at `(x, y)`
    /// in root-space (translated to CRTC-local coords here). The
    /// sole `set_cursor2(crtc, Some(dumb), …)` call site.
    ///
    /// # Errors
    /// `set_cursor2` or `move_cursor` ioctl failure; `NotFound` if
    /// `output_idx` is out of range or plane is unavailable.
    pub(crate) fn cursor_plane_show_on_crtc(
        &mut self,
        output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> io::Result<()> {
        let Some(layout) = self.outputs.get(output_idx) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such output"));
        };
        let crtc = layout.output.crtc;
        let layout_x = layout.x;
        let layout_y = layout.y;
        let Some(plane) = self.cursor_plane.as_mut() else {
            return Err(io::Error::other("cursor plane unavailable"));
        };
        let cx = x - layout_x - i32::from(hot_x);
        let cy = y - layout_y - i32::from(hot_y);
        plane.show(crtc, (i32::from(hot_x), i32::from(hot_y)), cx, cy)
    }

    /// Steady-state sprite-swap path. Re-issues `set_cursor2(Some,
    /// …)` ONLY on CRTCs whose plane state is already `visible`,
    /// followed by `move_to(x, y)` to restore the position. Hidden
    /// / pending CRTCs are untouched so the swap doesn't
    /// prematurely show on a CRTC mid-`Sw→Hw` transition.
    ///
    /// # Errors
    /// Aggregated — any per-CRTC ioctl failure is logged but does
    /// not abort the loop; only a missing plane returns `Err`.
    pub(crate) fn cursor_plane_rebind_visible_crtcs(
        &mut self,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> io::Result<()> {
        // Snapshot output layouts so the per-CRTC ioctls below can
        // borrow `&mut self.cursor_plane` exclusively.
        let layouts: Vec<(::drm::control::crtc::Handle, i32, i32)> = self
            .outputs
            .iter()
            .map(|l| (l.output.crtc, l.x, l.y))
            .collect();
        let Some(plane) = self.cursor_plane.as_mut() else {
            return Err(io::Error::other("cursor plane unavailable"));
        };
        for (crtc, layout_x, layout_y) in layouts {
            if !plane.is_visible_on(crtc) {
                continue;
            }
            let cx = x - layout_x - i32::from(hot_x);
            let cy = y - layout_y - i32::from(hot_y);
            if let Err(e) = plane.show(crtc, (i32::from(hot_x), i32::from(hot_y)), cx, cy) {
                log::warn!("v2 cursor rebind: show on {crtc:?} failed: {e}");
            }
        }
        Ok(())
    }

    /// `drmModeMoveCursor` per visible CRTC. Hidden CRTCs are
    /// skipped — the kernel naturally clips off-output coords on
    /// the visible ones, so no per-output geometry test is needed
    /// beyond the visibility filter.
    ///
    /// # Errors
    /// Logged per-CRTC; `Err` only when the plane is unavailable.
    pub(crate) fn cursor_plane_move(&mut self, x: i32, y: i32) -> io::Result<()> {
        // Snapshot first (see `cursor_plane_rebind_visible_crtcs`).
        let layouts: Vec<(::drm::control::crtc::Handle, i32, i32)> = self
            .outputs
            .iter()
            .map(|l| (l.output.crtc, l.x, l.y))
            .collect();
        let Some(plane) = self.cursor_plane.as_mut() else {
            return Err(io::Error::other("cursor plane unavailable"));
        };
        for (crtc, layout_x, layout_y) in layouts {
            if !plane.is_visible_on(crtc) {
                continue;
            }
            let cx = x - layout_x;
            let cy = y - layout_y;
            if let Err(e) = plane.move_to(crtc, cx, cy) {
                log::warn!("v2 cursor move on {crtc:?} failed: {e}");
            }
        }
        Ok(())
    }

    /// Detach the plane on a single CRTC. Output-local recovery
    /// (Phase D') uses this; the per-CRTC visibility map updates
    /// so subsequent rebind / move calls skip the CRTC cleanly.
    ///
    /// # Errors
    /// `NotFound` if `output_idx` is out of range or plane is
    /// unavailable; `set_cursor2` ioctl failure otherwise.
    pub(crate) fn cursor_plane_hide_on_crtc(&mut self, output_idx: usize) -> io::Result<()> {
        let Some(layout) = self.outputs.get(output_idx) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such output"));
        };
        let crtc = layout.output.crtc;
        let Some(plane) = self.cursor_plane.as_mut() else {
            return Err(io::Error::other("cursor plane unavailable"));
        };
        plane.hide(crtc)
    }

    /// Detach the plane on every CRTC the plane has ever been bound
    /// against AND every currently-known output. Global recovery
    /// fallback only — `drain_all`, shutdown, VT-leave, DRM-master
    /// loss. Per Phase D' this also invalidates `uploaded_version`
    /// so the next acquire/modeset re-uploads cleanly.
    ///
    /// # Errors
    /// Per-CRTC failures are logged; this never returns `Err`
    /// unless the plane is unavailable.
    pub(crate) fn cursor_plane_hide_all(&mut self) -> io::Result<()> {
        // Union of currently-tracked CRTCs and current output CRTCs.
        // Output disable could have removed a CRTC from `outputs`
        // while a stale visibility entry survives; iterate both.
        let mut crtcs: Vec<::drm::control::crtc::Handle> =
            self.outputs.iter().map(|l| l.output.crtc).collect();
        let Some(plane) = self.cursor_plane.as_mut() else {
            return Err(io::Error::other("cursor plane unavailable"));
        };
        for c in plane.known_crtcs() {
            if !crtcs.contains(&c) {
                crtcs.push(c);
            }
        }
        for crtc in crtcs {
            if let Err(e) = plane.hide(crtc) {
                log::warn!("v2 cursor hide_all on {crtc:?} failed: {e}");
            }
        }
        plane.invalidate_uploaded_version();
        Ok(())
    }

    pub(crate) fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.input_ctx.take()
    }

    pub(crate) fn poll_fds(&self) -> Vec<(RawFd, BackendFdKind)> {
        let mut fds = Vec::with_capacity(2);
        if let Some(ctx) = self.input_ctx.as_ref() {
            fds.push((ctx.fd(), BackendFdKind::Libinput));
        }
        fds.push((self.device.as_fd().as_raw_fd(), BackendFdKind::Drm));
        fds
    }

    pub(crate) fn drain_page_flip_events(&self) -> io::Result<Vec<usize>> {
        use ::drm::control::crtc;

        let mut flipped: Vec<crtc::Handle> = Vec::new();
        crate::drm::page_flip::drain_events(&self.device, |c| flipped.push(c))?;

        let mut output_indices = Vec::with_capacity(flipped.len());
        for crtc in flipped {
            let Some(output_idx) = self.outputs.iter().position(|o| o.output.crtc == crtc) else {
                log::warn!("v2: pageflip-complete for unknown CRTC {crtc:?}");
                continue;
            };
            output_indices.push(output_idx);
        }
        Ok(output_indices)
    }

    /// VkContext accessor for the engine. Returns `None` on the
    /// test fixture (`for_tests`) where Vk init is skipped.
    pub(crate) fn vk(&self) -> Option<&Arc<VkContext>> {
        self.vk.as_ref()
    }

    /// `OpsCommandPool` handle for the engine. `None` on the test
    /// fixture. Engine allocates per-op CBs from this pool.
    pub(crate) fn ops_command_pool_handle(&self) -> Option<vk::CommandPool> {
        self.ops_command_pool.as_ref().map(OpsCommandPool::handle)
    }

    // ── Storage allocation (Stage 2c) ───────────────────────────

    /// Sample-side view swizzle for a (format, depth) pair. The
    /// attachment-side view kept by `Storage::image_view` always
    /// uses IDENTITY (VUID-VkFramebufferCreateInfo-pAttachments-00891
    /// requires that for color attachments). The sample-side view
    /// kept by `Storage::sample_view` carries the format-aware
    /// swizzle so the scene compositor + engine sampling paths see
    /// X11-correct alpha semantics:
    ///
    /// - `(R8_UNORM, _)` → `a=R, rgb=ZERO` — R8 storage sampled as
    ///   an alpha mask (glyphs, RENDER mask scratch, depth-1 / 8
    ///   bitmaps). RGB channels intentionally zeroed so the
    ///   composite shader's `src * coverage` reads zero RGB and
    ///   the dst keeps its own colour.
    /// - `(B8G8R8A8_UNORM, depth == 24)` → `a=ONE` — depth-24
    ///   pixmaps (`PictFormat.alpha_mask = 0` per X11 RENDER spec)
    ///   must read α = 1.0 regardless of the BGRA8 padding byte.
    ///   Otherwise the scene's `alpha_passthrough=true` window
    ///   draws blend with undefined α and the layer below leaks
    ///   through.
    /// - everything else → IDENTITY (depth-32 ARGB passes α
    ///   through; unknown formats default-safe).
    ///
    /// Mirrors `engine::swizzle_class_for` (the engine's RENDER
    /// view-cache classifier) — the engine cache stays for the
    /// cases where the sampler config also differs; this helper
    /// owns the storage-side view that the scene compositor
    /// binds directly.
    pub(crate) fn sample_view_components(format: vk::Format, depth: u8) -> vk::ComponentMapping {
        match (format, depth) {
            (vk::Format::R8_UNORM, _) => vk::ComponentMapping {
                r: vk::ComponentSwizzle::ZERO,
                g: vk::ComponentSwizzle::ZERO,
                b: vk::ComponentSwizzle::ZERO,
                a: vk::ComponentSwizzle::R,
            },
            (vk::Format::B8G8R8A8_UNORM, 24) => vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::ONE,
            },
            _ => vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::IDENTITY,
            },
        }
    }

    /// Build a fresh sample-side `vk::ImageView` over `image` with
    /// the format/depth-aware swizzle from
    /// [`Self::sample_view_components`]. Used by the fresh-alloc
    /// path, the pool-take path (where the pool only stores the
    /// attachment view), and the DRI3 import path (where the
    /// imported DrawableImage carries an identity-swizzle view we
    /// can't reuse for scene sampling).
    pub(crate) fn build_sample_view(
        vk: &crate::kms::vk::device::VkContext,
        image: vk::Image,
        format: vk::Format,
        depth: u8,
    ) -> Result<vk::ImageView, vk::Result> {
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(Self::sample_view_components(format, depth))
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        unsafe { vk.device.create_image_view(&info, None) }
    }

    /// Map an X11 drawable depth to its v2 storage format. Mirrors
    /// `DrawableImage::format_for_pixmap_depth` (v1) so the two
    /// don't drift.
    #[must_use]
    pub(crate) fn format_for_depth(depth: u8) -> vk::Format {
        match depth {
            1 | 8 => vk::Format::R8_UNORM,
            24 | 32 => vk::Format::B8G8R8A8_UNORM,
            other => {
                log::warn!(
                    "v2 PlatformBackend::format_for_depth: unhandled depth {other} → \
                     defaulting to B8G8R8A8_UNORM",
                );
                vk::Format::B8G8R8A8_UNORM
            }
        }
    }

    /// Allocate a fresh server-owned [`Storage`] for the
    /// [`DrawableStore`]. DEVICE_LOCAL memory; tiling=OPTIMAL;
    /// usage covers Stage 2c (TRANSFER_SRC/DST, COLOR_ATTACHMENT,
    /// SAMPLED). Initial layout = `UNDEFINED`.
    ///
    /// # Errors
    ///
    /// Returns `ERROR_INITIALIZATION_FAILED` if Vk is not
    /// available (test fixture). Propagates `vkCreateImage` /
    /// `vkAllocateMemory` / `vkBindImageMemory` /
    /// `vkCreateImageView` failures.
    pub(crate) fn allocate_drawable_storage(
        &self,
        width: u16,
        height: u16,
        depth: u8,
    ) -> Result<Storage, vk::Result> {
        let vk = self
            .vk
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let format = Self::format_for_depth(depth);
        let extent = vk::Extent2D {
            width: u32::from(width.max(1)),
            height: u32::from(height.max(1)),
        };

        // Stage 3f.10: try the recycle pool before falling through to
        // a fresh Vk allocate. v1's pool keys on
        // (width, height, format); the usage flag set is constant
        // across all server-owned pixmaps (matches v1).
        if let Some(pool) = self.pixmap_pool.as_ref() {
            let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
                width: extent.width,
                height: extent.height,
                format,
            };
            if let Some(pooled) = pool.try_take(key) {
                // The pool stores only the attachment-side
                // (IDENTITY) view; the sample-side view is
                // depth-specific (a recycled depth-32 BGRA8
                // image can serve a fresh depth-24 request and
                // vice versa, since the pool key is format only),
                // so always build a fresh sample_view for the
                // current request's depth. View creation is cheap;
                // pooling the image + memory is where the win is.
                let pooled_image = pooled.image;
                let sample_view = match Self::build_sample_view(vk, pooled_image, format, depth) {
                    Ok(v) => v,
                    Err(e) => {
                        // Couldn't build a sample_view: return the
                        // pooled triple back to the pool and fall
                        // through to fresh allocate (which also
                        // tries to build a sample_view and may also
                        // fail — but the diagnostic path is
                        // uniform that way).
                        let _ = pool.try_return(key, pooled);
                        return Err(e);
                    }
                };
                return Ok(Storage::from_pooled(
                    pooled,
                    sample_view,
                    extent,
                    format,
                    depth,
                ));
            }
        }

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&image_info, None)? };

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let memory_type_index = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let Some(mt) = memory_type_index else {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e);
            }
        };
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e);
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
        };

        // Sample-side view with format/depth-aware swizzle. The
        // scene compositor and the engine view-cache fall back to
        // this view for sampling instead of `view` (IDENTITY) so
        // depth-24 BGRA8 storage reads α=ONE per X11 PictFormat
        // semantics. Built unconditionally — for depth-32 the
        // swizzle is identity, but a distinct VkImageView keeps
        // Storage's ownership story uniform.
        let sample_view = match Self::build_sample_view(vk, image, format, depth) {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.destroy_image_view(view, None);
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
        };

        Ok(Storage::new_server_owned(
            image,
            memory,
            view,
            sample_view,
            extent,
            format,
            depth,
        ))
    }

    /// Submit a paint command buffer with `signal_fence`. Stage 2c
    /// covers paint-only submits (no KMS sync semaphore); Stage 2d's
    /// compose path adds the semaphore parameter.
    ///
    /// # Errors
    ///
    /// Propagates `vkQueueSubmit2` failures. Sets `renderer_failed`
    /// on Err so the next op surfaces the condition.
    pub(crate) fn submit_paint_cb(
        &mut self,
        cb: vk::CommandBuffer,
        signal_fence: vk::Fence,
    ) -> Result<(), vk::Result> {
        let Some(vk) = self.vk.as_ref() else {
            return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
        crate::vk_count!(queue_submit2);
        match unsafe {
            vk.device
                .queue_submit2(vk.graphics_queue, &submit, signal_fence)
        } {
            Ok(()) => Ok(()),
            Err(e) => {
                self.renderer_failed = true;
                Err(e)
            }
        }
    }

    // ── I6a: FenceTicket primitives ─────────────────────────────

    /// Acquire a fresh, unsignaled fence. Caller passes
    /// `ticket.fence()` to `vkQueueSubmit2` as the signal fence.
    /// Cloned across consumers; final-drop recycles or leaks.
    ///
    /// # Errors
    ///
    /// Returns `Err` if Vk is not initialised (test fixture) or
    /// fence creation fails.
    pub(crate) fn acquire_fence_ticket(&self) -> Result<FenceTicket, vk::Result> {
        let pool = self
            .fence_pool
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        pool.acquire()
    }

    // ── I6b: scanout BO management ──────────────────────────────

    /// Pick the next BO to render into for `output_idx`, or
    /// `None` if all BOs are still in flight (the SceneCompositor
    /// should retry next core-loop iteration).
    ///
    /// The token carries `last_present_generation` and
    /// `content_invalidated` so the buffer-age algorithm in
    /// SceneCompositor doesn't need to reach into the pool.
    pub(crate) fn acquire_scanout_bo(&mut self, output_idx: usize) -> Option<ScanoutBoToken> {
        let pool = self.scanout_pools.get_mut(output_idx)?.as_mut()?;
        let gens = self.bo_generations.get(output_idx)?;
        for (bo_idx, bo) in pool.bos.iter().enumerate() {
            if bo.state.phase == BoPhase::Free {
                let entry = gens.get(bo_idx).copied().unwrap_or_default();
                return Some(ScanoutBoToken {
                    output_idx,
                    bo_idx,
                    extent: vk::Extent2D {
                        width: bo.width,
                        height: bo.height,
                    },
                    last_present_generation: entry.last_present_generation,
                    content_invalidated: entry.content_invalidated,
                });
            }
        }
        None
    }

    /// Mark a BO's content tracking as invalidated. Called by
    /// SceneCompositor on the 9b atomic-commit-failed path —
    /// the GPU rendered into the BO but KMS rejected the flip,
    /// so the BO contents are indeterminate.
    pub(crate) fn invalidate_bo(&mut self, output_idx: usize, bo_idx: usize) {
        if let Some(gens) = self.bo_generations.get_mut(output_idx)
            && let Some(g) = gens.get_mut(bo_idx)
        {
            g.content_invalidated = true;
            g.last_present_generation = None;
        }
    }

    /// Recycle a scanout BO whose GPU work was submitted but whose
    /// atomic commit was rejected. The caller must only invoke this
    /// after the compose fence has signaled, otherwise the BO could
    /// be rendered into again while the previous command buffer is
    /// still writing it.
    pub(crate) fn recycle_failed_submit_bo(&mut self, output_idx: usize, bo_idx: usize) {
        let Some(bo) = self
            .scanout_pools
            .get_mut(output_idx)
            .and_then(Option::as_mut)
            .and_then(|pool| pool.bos.get_mut(bo_idx))
        else {
            return;
        };
        bo.state = BoState::default();
    }

    /// Called by the SceneCompositor's tick after `present_scanout`
    /// returns Ok. Records that `bo_idx` is now pending the next
    /// page-flip-complete event for `output_idx`, and assigns the
    /// generation number for the in-flight frame.
    ///
    /// Returns the freshly-allocated generation.
    pub(crate) fn record_present(&mut self, _output_idx: usize, _bo_idx: usize) -> u64 {
        self.next_present_generation = self
            .next_present_generation
            .checked_add(1)
            .expect("next_present_generation overflow");
        self.next_present_generation
    }

    /// Page-flip-complete callback. Walks the output's BOs, finds
    /// the one currently `Pending` (just retired by the kernel),
    /// transitions its state, and returns the retirement info.
    /// `None` means no flip was pending — a spurious or
    /// startup-flushed event.
    ///
    /// The caller (SceneCompositor) then advances the matching
    /// `bo_generations[output_idx][bo_idx].last_present_generation`
    /// via [`Self::commit_bo_present`].
    pub(crate) fn on_page_flip_complete(
        &mut self,
        output_idx: usize,
    ) -> Option<PageFlipRetirement> {
        let pool = self.scanout_pools.get_mut(output_idx)?.as_mut()?;
        // First pass: find any BO currently `Pending`. Walk only
        // — don't mutate during the search.
        let mut pending: Option<usize> = None;
        let mut on_screen: Option<usize> = None;
        for (i, bo) in pool.bos.iter().enumerate() {
            match bo.state.phase {
                BoPhase::Pending => {
                    if let Some(prev) = pending {
                        // More than one pending — shouldn't
                        // happen; the kernel flips one at a time.
                        log::warn!(
                            "v2 on_page_flip_complete: output {output_idx} has >1 pending BO; \
                             retiring first found ({prev})",
                        );
                    } else {
                        pending = Some(i);
                    }
                }
                BoPhase::OnScreen => {
                    on_screen = Some(i);
                }
                _ => {}
            }
        }
        let presented = pending?;
        // Transitions:
        //   - the previously OnScreen bo goes Retiring → Free
        //   - the previously Pending bo goes OnScreen
        // Doing it in this order matches v1's compositor.
        let retired = if let Some(prev) = on_screen {
            pool.bos[prev].state.transition_to_retiring();
            let released = pool.bos[prev].state.transition_to_free_after_retire();
            if let Some(fd) = released {
                // SAFETY: the release fence fd was owned by us;
                // close it now that the BO is free.
                unsafe { libc::close(fd) };
            }
            Some(prev)
        } else {
            None
        };
        pool.bos[presented].state.transition_to_on_screen();

        let logged_first = self
            .first_pageflip_logged
            .get_mut(output_idx)
            .map(|f| std::mem::replace(f, true))
            .unwrap_or(true);
        if !logged_first {
            log::info!("v2: first pageflip complete on output {output_idx} (bo {presented})",);
        } else {
            log::debug!("v2: pageflip complete on output {output_idx} (bo {presented})",);
        }
        Some(PageFlipRetirement {
            retired_bo_idx: retired,
            presented_bo_idx: presented,
            generation: 0, // assigned by record_present; this is informational
        })
    }

    /// SceneCompositor calls this on page-flip-complete after
    /// `on_page_flip_complete` to write the new
    /// `last_present_generation` and clear `content_invalidated`.
    pub(crate) fn commit_bo_present(&mut self, output_idx: usize, bo_idx: usize, generation: u64) {
        if let Some(gens) = self.bo_generations.get_mut(output_idx)
            && let Some(g) = gens.get_mut(bo_idx)
        {
            g.last_present_generation = Some(generation);
            g.content_invalidated = false;
        }
    }

    // ── Disable output ──────────────────────────────────────────

    /// Post-loop teardown — disable each output, leaving the
    /// scanout BOs in a state where their Drop can clean up
    /// (or, on atomic disable failure, disarm them so we leak
    /// rather than confuse KMS — same shape as v1).
    ///
    /// # Errors
    ///
    /// Propagates the first per-output `disable_output` failure;
    /// subsequent outputs still attempted.
    pub(crate) fn disable_output(&mut self) -> io::Result<()> {
        self.shutting_down = true;

        // Best-effort: drain all in-flight GPU work before
        // pulling the modeset.
        if let Some(vk) = self.vk.as_ref() {
            unsafe {
                let _ = vk.device.device_wait_idle();
            }
        }

        // Stage 3f.10: drain the pixmap pool so the recycled
        // image/memory/view triples don't leak through the
        // VkContext destruction path. Safe to drain here: every
        // in-flight CB has been waited on by device_wait_idle.
        if let Some(pool) = self.pixmap_pool.as_ref() {
            pool.drain();
        }

        let mut first_err: Option<io::Error> = None;
        for (i, layout) in self.outputs.iter().enumerate() {
            if let Err(e) = drm::modeset::disable_output(&self.device, &layout.output) {
                log::warn!(
                    "v2 disable_output: failed for {} (output {i}): {e}",
                    layout.output.connector_name,
                );
                // Disarm the matching scanout pool so its Drop
                // doesn't try to destroy framebuffers KMS may
                // still hold (matches v1's behaviour).
                if let Some(pool) = self.scanout_pools.get_mut(i).and_then(|p| p.as_mut()) {
                    for bo in &mut pool.bos {
                        bo.disarm();
                    }
                }
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test fixture works at all: open `for_tests`, query
    /// dimensions, query poll_fds, no Vk required.
    #[test]
    fn for_tests_constructs() {
        let p = PlatformBackend::for_tests();
        assert_eq!(p.fb_dimensions(), (800, 600));
        assert_eq!(p.outputs.len(), 1);
        assert!(p.vk.is_none()); // for_tests skips Vk
        let fds = p.poll_fds();
        // No input_ctx, one DRM fd.
        assert!(fds.iter().any(|(_, k)| matches!(k, BackendFdKind::Drm)));
    }

    /// Fence acquire on a no-Vk fixture returns the
    /// "init failed" error (since fence_pool is None). This
    /// confirms the guard is wired; real fence allocation is
    /// covered by Stage 2c+ Vk-backed tests.
    #[test]
    fn for_tests_fence_acquire_errors_without_vk() {
        let p = PlatformBackend::for_tests();
        let result = p.acquire_fence_ticket();
        assert!(matches!(
            result,
            Err(vk::Result::ERROR_INITIALIZATION_FAILED)
        ));
    }

    /// BO acquire on a no-Vk fixture returns None (the single
    /// stub output has no pool).
    #[test]
    fn for_tests_scanout_acquire_returns_none() {
        let mut p = PlatformBackend::for_tests();
        assert!(p.acquire_scanout_bo(0).is_none());
    }

    /// `invalidate_bo` on a missing entry is a no-op (doesn't
    /// panic). With no pool entries there's nothing to flag,
    /// but the call must remain safe.
    #[test]
    fn for_tests_invalidate_bo_is_noop_on_missing_entry() {
        let mut p = PlatformBackend::for_tests();
        p.invalidate_bo(0, 0); // empty bo_generations[0]
        p.invalidate_bo(99, 0); // out-of-range output_idx
    }

    /// `on_page_flip_complete` without a prior `present_scanout`
    /// is a no-op (no Pending BO to retire).
    #[test]
    fn for_tests_on_page_flip_complete_without_pending_is_none() {
        let mut p = PlatformBackend::for_tests();
        assert!(p.on_page_flip_complete(0).is_none());
    }

    /// `record_present` advances `next_present_generation`
    /// monotonically.
    #[test]
    fn record_present_advances_generation() {
        let mut p = PlatformBackend::for_tests();
        let g1 = p.record_present(0, 0);
        let g2 = p.record_present(0, 0);
        assert_eq!(g1 + 1, g2);
        assert!(g1 > 0); // first generation is 1, not 0
    }

    /// `commit_bo_present` is a no-op on a missing entry, but
    /// the `record_present` counter still advances and survives
    /// a subsequent successful entry write.
    #[test]
    fn commit_bo_present_is_safe_on_missing_entry() {
        let mut p = PlatformBackend::for_tests();
        let g = p.record_present(0, 0);
        p.commit_bo_present(0, 0, g); // bo_generations[0] is empty — no-op
        p.commit_bo_present(99, 99, g); // out-of-range — no-op
    }
}
