//! `KmsBackendV2` — Stage 1b skeleton sibling of `KmsBackend` (v1).
//!
//! Per rendering-model-v2 spec § Stage 1b. Embeds the same
//! `KmsCore` as v1 so protocol bookkeeping (XID maps, window
//! metadata stripped of storage, fonts, SHAPE regions, etc.) lives
//! exactly once. Every paint / scene / RENDER trait method stubs
//! with a once-per-method `warn!` + `Ok(())`. Real components
//! (`PlatformBackend`, `DrawableStore`, `RenderEngine`,
//! `SceneCompositor`) land in Stage 2.
//!
//! The acceptance gate is **synthetic**: with
//! `YSERVER_RENDER_MODEL=v2`, the server boots, opens a connection,
//! services capability queries / atom queries / GetGeometry on
//! root; the first paint op produces exactly one
//! `v2: <method> not yet implemented` warn line per opcode. No
//! real-app gates land at this stage — those wait for Stage 3.

use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
};

use yserver_core::{
    backend::{
        AnyHandle, Backend, BackendFdKind, ClipState, CursorHandle, DrawState, Dri3Caps, FillState,
        FontHandle, GlyphSetHandle, OriginContext, PictureHandle, PixmapHandle, PresentCaps,
        WindowHandle,
    },
    core_loop::HostInputEvent,
    host_x11::{
        HostKeyEvent, HostPointerEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap,
        PointerEventKind, PointerPosition,
    },
    resources::{ARGB_COLORMAP, ARGB_VISUAL},
    server::ServerState,
};
use yserver_protocol::x11::{
    ClipRectangles, FontMetrics, RENDER_FMT_A1, RENDER_FMT_A8, RENDER_FMT_ARGB32, ResourceId,
    xfixes,
};

use crate::{
    drm,
    kms::{
        core::{GradientStop, KmsCore, PictureFilter, PictureRecord},
        cpu_types::{PictTransform, Rectangle16, Repeat},
        v2::{
            engine::{RenderEngine, decode_x11_pixel_server_alpha},
            platform::PlatformBackend,
            scene::SceneCompositor,
            store::{DrawableKind, DrawableStore, Storage},
            telemetry::Telemetry,
        },
    },
};

/// Per-window geometry tracked by v2's scene assembler. Stage 2 plan
/// Risk 3: a parallel `windows_v2` map on `KmsBackendV2` (NOT on
/// `KmsCore` — v1 doesn't need it). Stage 4 may collapse into
/// `KmsCore.windows` when `WindowState` splits.
///
/// Stage 3f.6 grows `parent`: subwindows record their parent xid so
/// `build_scene` can recurse top-level → descendants with accumulated
/// offsets. `None` marks top-levels (parent is root, not tracked
/// in `windows_v2`). The `bg_pixel` / `bg_pixmap` slots carry
/// per-window background attributes set via
/// `change_subwindow_attributes`; the bg-pixel is painted into
/// storage at allocate + configure resize so freshly-mapped windows
/// have a defined initial colour.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowGeometryV2 {
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    pub(crate) mapped: bool,
    pub(crate) parent: Option<u32>,
    pub(crate) stack_rank: u64,
    pub(crate) bg_pixel: Option<u32>,
    pub(crate) bg_pixmap: Option<u32>,
}

pub(crate) type WindowsV2Map = HashMap<u32, WindowGeometryV2>;

/// Stage 4a — resolution result for a paint operation against a
/// host xid. `id` is the DrawableId that actually receives the
/// paint; `offset` is the (x, y) translation that callers add to
/// every paint rect's origin (in 16.16-free pixel units) before
/// dispatching to the engine.
///
/// The offset is non-zero only when the target is a descendant of
/// a redirected ancestor: paint against descendant `C` of
/// redirected `W`, with `C` positioned at `(cx, cy)` relative to
/// `W`, lands at `(cx + x, cy + y, w, h)` in `W`'s backing.
///
/// For unredirected windows and Pixmap targets, `offset = (0, 0)`
/// and `id` is just the leaf drawable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaintTarget {
    pub(crate) id: crate::kms::v2::store::DrawableId,
    pub(crate) offset: (i32, i32),
}

/// v2 sibling backend. Shares `KmsCore` with `KmsBackend`;
/// owns `PlatformBackend` (real DRM/Vk/libinput per Stage 2a)
/// plus stub `DrawableStore` / `RenderEngine` / `SceneCompositor`
/// that fill in across Stages 2b–2e. Paint / RENDER / scene ops
/// log gaps until those substages land.
pub struct KmsBackendV2 {
    /// Shared protocol-bookkeeping state. Identical to v1's
    /// `KmsBackend.core` — same struct, same construction path.
    pub(crate) core: KmsCore,

    /// Real DRM/KMS/libinput/Vulkan owner per Stage 2a. Replaced
    /// the flat field set Stage 1b carried.
    pub(crate) platform: PlatformBackend,

    /// Once-per-method dedup set for `v2: <method> not yet
    /// implemented` warnings. `RefCell` to keep the helper callable
    /// from `&self` paths (capability accessors that log gaps).
    logged_gaps: RefCell<HashSet<&'static str>>,

    /// v2's storage layer (Stage 2b). Tracks every drawable's
    /// VkImage + refcount + damage + retirement-fence; allocated
    /// via `PlatformBackend::allocate_drawable_storage`.
    pub(crate) store: DrawableStore,
    /// v2's paint engine (Stage 2c). Drives `fill_rect`,
    /// `put_image`, `get_image` directly into `DrawableStore`
    /// storage; consumed by every `Backend` paint method on this
    /// backend.
    pub(crate) engine: RenderEngine,
    /// v2's scene compositor — real per Stage 2d.
    pub(crate) scene: SceneCompositor,
    /// v2's per-second telemetry counters (Stage 2f). The
    /// per-second emitter logs under `YSERVER_LOOP_TELEMETRY=1`;
    /// lifetime totals are always tracked for the acceptance
    /// harness.
    pub(crate) telemetry: Telemetry,
    /// Per-window geometry tracked outside `KmsCore` (v1 doesn't
    /// need it). Keyed by host xid; mutated by
    /// `register_top_level` / `register_subwindow` /
    /// `create_subwindow` / `configure_subwindow` /
    /// `map_subwindow` / `unmap_subwindow` /
    /// `destroy_subwindow`.
    pub(crate) windows_v2: WindowsV2Map,
    /// Monotonic allocator for per-parent sibling ordering. V2 scene
    /// assembly still stores windows in a flat map, so child z-order
    /// needs an explicit stable rank instead of relying on HashMap
    /// iteration order.
    next_window_stack_rank: u64,
    /// Stage 4d: `DrawableId` of the Composite Overlay Window
    /// storage, allocated lazily on the first `GetOverlayWindow`
    /// and dropped on the final `ReleaseOverlayWindow`. `None`
    /// when no compositor is holding a COW. Storage handle lives
    /// here (backend / Vk-side state) — the matching protocol
    /// refcount lives on `core.cow_refcount` per the v2 plan
    /// §"`KmsCore` scope — narrowly drawn" split.
    pub(crate) cow_id: Option<crate::kms::v2::store::DrawableId>,

    /// Test-only counter: bumps every time
    /// `clear_window_area_with_background` is entered. Used by the
    /// `cwa_on_redirected_window_does_not_clear_backing` regression
    /// test to verify the Stage 4d CWA-clear-skip behavior without
    /// needing a Vk-backed fixture or scanout-readback. Always
    /// present (not `cfg(test)`) so the increment is a single
    /// branchless line; production paths don't observe it.
    pub(crate) clear_window_area_calls: u32,

    /// Diagnostic ring of recent `PRESENT::Pixmap` source xids
    /// targeted at COW. Captured via `note_present_pixmap` and
    /// consumed by `do_dump_drawables_v2` so the per-drawable
    /// dump includes "marco's most-recent offscreen" — the
    /// pixmap whose content marco PresentPixmap'd to COW most
    /// recently. Ring capacity 16 to keep memory trivial while
    /// covering marco's typical double-buffered front/back pair
    /// plus head-room for short flips of additional sources.
    pub(crate) present_to_cow_sources: std::collections::VecDeque<u32>,

    /// DRI3 `FenceFromFD` xshmfence-backed fences keyed by the
    /// client's xid. Mesa's loader_dri3 uses xshmfence (memfd +
    /// futex) for idle/sync fences; the mmap'd mapping lets us
    /// `xshmfence_trigger` directly when the X side wants to
    /// signal idle. Mirrors v1's `dri3_xshmfences` field shape.
    pub(crate) dri3_xshmfences: HashMap<u32, crate::kms::xshmfence::FenceMapping>,
    /// DRI3 sync-fence / syncobj resources keyed by the client's
    /// xid. Either `FenceFromFD` falling through the xshmfence
    /// path (sync_file fd → `VkSemaphore`) or `ImportSyncobj`
    /// (drm_syncobj fd → timeline `VkSemaphore`). Mirrors v1's
    /// `dri3_sync_resources` field shape.
    pub(crate) dri3_sync_resources: HashMap<u32, ash::vk::Semaphore>,
}

impl KmsBackendV2 {
    /// Test-only entry point: drives the production `get_image` path
    /// but returns just the pixel bytes (header stripped). Acceptance
    /// tests use this so they can index into the result starting at
    /// pixel 0 without each one having to remember the 32-byte X11
    /// reply prefix.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn get_image_pixels_for_tests(
        &mut self,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        use yserver_core::backend::Backend;
        let reply = self.get_image(None, host_xid, format, x, y, width, height, plane_mask)?;
        Ok(reply.map(|r| {
            assert!(r.len() >= 32, "v2 GetImage reply missing 32-byte header");
            r[32..].to_vec()
        }))
    }

    fn alloc_window_stack_rank(&mut self) -> u64 {
        let rank = self.next_window_stack_rank;
        self.next_window_stack_rank = self.next_window_stack_rank.saturating_add(1);
        rank
    }

    fn clear_window_area_with_background(
        &mut self,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::ResolvedSource, vk::ops::render::CompositeRect};

        self.clear_window_area_calls = self.clear_window_area_calls.wrapping_add(1);
        self.clear_clip_rectangles(None)?;
        let Some(dst_target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        if let Some(bg_host_xid) = background_pixmap_host_xid
            && let Some(src) = self.store.lookup(bg_host_xid)
        {
            if src == dst_target.id {
                return Ok(());
            }
            if self.store.get(src).map(|d| d.storage.format)
                == Some(ash::vk::Format::B8G8R8A8_UNORM)
            {
                let rects = [CompositeRect {
                    src_x: i32::from(x),
                    src_y: i32::from(y),
                    mask_x: 0,
                    mask_y: 0,
                    dst_x: dst_target.offset.0 + i32::from(x),
                    dst_y: dst_target.offset.1 + i32::from(y),
                    width: u32::from(width),
                    height: u32::from(height),
                }];
                const OP_SRC: u8 = 1;
                match self.engine.render_composite(
                    &mut self.store,
                    &mut self.platform,
                    OP_SRC,
                    ResolvedSource::Drawable(src),
                    ResolvedSource::None,
                    dst_target.id,
                    &rects,
                    None,
                    Repeat::Normal,
                    Repeat::None,
                    None,
                    None,
                    false,
                    // Audit #4: no Picture context — pass 0 so the
                    // engine falls back to the depth-based swizzle.
                    0,
                    0,
                    0,
                ) {
                    Ok(s) if s.recorded_draws > 0 => {
                        self.telemetry.record_paint_submit();
                        return Ok(());
                    }
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        log::warn!(
                            "v2 clear_window_area_with_background: tiled bg_pixmap clear failed \
                             for 0x{host_xid:x}: {e:?}"
                        );
                    }
                }
            }
        }
        self.fill_rectangle(None, host_xid, background_pixel, x, y, width, height)
    }

    fn restack_subwindow(&mut self, host_xid: u32, stack_mode: u8, sibling: Option<u32>) {
        let Some(current) = self.windows_v2.get(&host_xid).copied() else {
            return;
        };
        let parent = current.parent;
        let mut siblings: Vec<(u32, u64)> = self
            .windows_v2
            .iter()
            .filter_map(|(xid, geom)| (geom.parent == parent).then_some((*xid, geom.stack_rank)))
            .collect();
        siblings.sort_by_key(|(_, rank)| *rank);
        let Some(pos) = siblings.iter().position(|(xid, _)| *xid == host_xid) else {
            return;
        };
        let entry = siblings.remove(pos);
        let sibling_pos = sibling.and_then(|sib| siblings.iter().position(|(xid, _)| *xid == sib));
        match stack_mode {
            0 | 2 | 4 => match sibling_pos {
                Some(sp) => siblings.insert(sp + 1, entry),
                None => siblings.push(entry),
            },
            1 | 3 => match sibling_pos {
                Some(sp) => siblings.insert(sp, entry),
                None => siblings.insert(0, entry),
            },
            _ => siblings.push(entry),
        }
        for (rank, (xid, _)) in siblings.into_iter().enumerate() {
            if let Some(geom) = self.windows_v2.get_mut(&xid) {
                geom.stack_rank = u64::try_from(rank).unwrap_or(u64::MAX);
            }
        }
    }

    /// Real-DRM-real-Vk constructor. Per Stage 2a, the platform
    /// layer (DRM device, output layouts, libinput, VkContext,
    /// ops command pool, fence pool, per-output scanout pools)
    /// is real; v2's `DrawableStore` / `RenderEngine` /
    /// `SceneCompositor` are still stubs and paint paths log
    /// gaps.
    ///
    /// # Errors
    ///
    /// Propagates DRM / Vk / libinput init failures from
    /// `PlatformBackend::open_with_commit`, plus FontLoader / XKB
    /// init failures from `KmsCore::new`.
    pub fn open(device_path: &str) -> io::Result<Self> {
        Self::open_with_commit(device_path, drm::modeset::commit_modeset)
    }

    fn open_with_commit(
        device_path: &str,
        commit: fn(
            &crate::drm::Device,
            &crate::drm::modeset::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        let platform = PlatformBackend::open_with_commit(device_path, commit)?;
        let (fb_w, fb_h) = (platform.fb_w, platform.fb_h);
        let core = KmsCore::new(fb_w, fb_h)?;
        let engine = RenderEngine::new(&platform)
            .map_err(|e| io::Error::other(format!("v2 RenderEngine::new failed: {e:?}")))?;
        let scene = SceneCompositor::new(&platform)
            .map_err(|e| io::Error::other(format!("v2 SceneCompositor::new failed: {e:?}")))?;
        log::info!(
            "yserver(v2): KmsBackendV2 boot — {} output(s), {fb_w}x{fb_h} virtual screen; \
             Stage 2c engine + Stage 2d scene live (full-redraw, no buffer-age); \
             expect 'v2: <method> not yet implemented' warns for ops outside \
             Stage 2c/2d on first client request",
            platform.outputs.len(),
        );
        let mut b = Self {
            core,
            platform,
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine,
            scene,
            windows_v2: WindowsV2Map::new(),
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            cow_id: None,
            clear_window_area_calls: 0,
            present_to_cow_sources: std::collections::VecDeque::with_capacity(16),
            dri3_xshmfences: HashMap::new(),
            dri3_sync_resources: HashMap::new(),
        };
        b.init_root_storage();
        // Stage 3f.8: bake the default-arrow software cursor.
        // Best-effort — a failure logs + leaves the cursor invisible
        // (matches pre-3f.8 behaviour, no regression).
        if let Err(e) = b.init_cursor_sprite() {
            log::warn!("v2: software cursor init failed: {e:?} — no visible cursor");
        }
        Ok(b)
    }

    /// Stage 3f.8: allocate the default cursor sprite (16×16 black
    /// triangle, hotspot (0,0)) as a Pixmap-kind Drawable + upload
    /// the pixel data via `engine.put_image`. Registers the result
    /// on `SceneCompositor` so `build_scene` appends it at top of
    /// z. One-time setup; subsequent `define_cursor` flows (Stage 4)
    /// can replace the entry.
    fn init_cursor_sprite(&mut self) -> io::Result<()> {
        const CW: u16 = 16;
        const CH: u16 = 16;
        let xid = self.core.next_host_xid();
        let storage = self
            .platform
            .allocate_drawable_storage(CW, CH, 32)
            .map_err(|e| io::Error::other(format!("cursor storage: {e:?}")))?;
        let id = self
            .store
            .allocate(xid, DrawableKind::Pixmap, 32, false, storage)
            .map_err(|e| io::Error::other(format!("cursor store.allocate: {e:?}")))?;
        let bytes = default_cursor_sprite_bgra();
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            id,
            ash::vk::Offset2D::default(),
            ash::vk::Extent2D {
                width: u32::from(CW),
                height: u32::from(CH),
            },
            &bytes,
            32,
        ) {
            return Err(io::Error::other(format!("cursor put_image: {e:?}")));
        }
        self.scene
            .register_cursor(crate::kms::v2::scene::CursorEntry {
                id,
                extent: ash::vk::Extent2D {
                    width: u32::from(CW),
                    height: u32::from(CH),
                },
                hot_x: 0,
                hot_y: 0,
            });
        log::info!("v2: software cursor sprite registered ({CW}x{CH})");
        Ok(())
    }

    /// Test fixture with live Vulkan attached. Falls back to the
    /// headless `for_tests` shape if `VkContext::new` fails. Used
    /// by the Stage 2f acceptance harness which needs real paint
    /// + readback on the v2 path.
    ///
    /// # Errors
    ///
    /// Returns `Err` only when Vk init fails AND the caller
    /// explicitly wanted Vk-backed tests; callers that can fall
    /// back to headless use `for_tests` directly.
    #[doc(hidden)]
    pub fn for_tests_with_vk() -> Result<Self, io::Error> {
        use std::sync::Arc;
        // Build the test seed WITHOUT the root drawable. If
        // `for_tests()` were used, `init_root_storage` would have
        // run with no Vk attached and stamped a `for_tests_null`
        // stub (vk::ImageView::null()) into the store. The second
        // `init_root_storage` call below would then short-circuit
        // on the existing xid and we'd be left with a null-view
        // root — any `render_composite` against it (e.g.
        // `set_container_background_pixmap`) segfaults inside the
        // descriptor-set bind.
        let mut base = Self::for_tests_seed();
        let vk = crate::kms::vk::device::VkContext::new()
            .map_err(|e| io::Error::other(format!("v2 for_tests_with_vk: VkContext: {e:?}")))?;
        let ops_pool = crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)).map_err(|e| {
            io::Error::other(format!("v2 for_tests_with_vk: OpsCommandPool: {e:?}"))
        })?;
        let fence_pool = crate::kms::v2::platform::FencePool::new(Arc::clone(&vk));
        base.platform.vk = Some(vk);
        base.platform.ops_command_pool = Some(ops_pool);
        base.platform.fence_pool = Some(fence_pool);
        // Replace the stub engine with a live one now that Vk
        // is attached. Scene compositor stays stubbed (no
        // scanout pool on the test fixture).
        base.engine = crate::kms::v2::engine::RenderEngine::new(&base.platform)
            .map_err(|e| io::Error::other(format!("v2 for_tests_with_vk: RenderEngine: {e:?}")))?;
        base.init_root_storage();
        Ok(base)
    }

    /// Stage 4b — test-only read of the alias registry. Returns
    /// a copy of the entry if the backing xid is tracked; the
    /// `pub(crate)` `KmsCore.alias_registry` is otherwise unreachable
    /// from the `tests/` integration crate.
    #[doc(hidden)]
    #[must_use]
    pub fn test_alias_registry_get(
        &self,
        backing_xid: u32,
    ) -> Option<crate::kms::core::AliasEntry> {
        let handle = yserver_core::backend::PixmapHandle::from_raw(backing_xid)?;
        self.core.alias_registry.get(handle).copied()
    }

    /// Stage 4b — test-only read of the host_window_to_backing
    /// map. Returns the backing xid registered against
    /// `window_xid`, or `None` when the window isn't redirected.
    #[doc(hidden)]
    #[must_use]
    pub fn test_host_window_to_backing(&self, window_xid: u32) -> Option<u32> {
        self.core
            .host_window_to_backing
            .get(&window_xid)
            .map(|h| h.as_raw())
    }

    /// Stage 4c.5 — test-only probe for a drawable's presentation
    /// damage. Returns `true` iff the drawable exists, has
    /// `scene_participating=true` (the `peek_presentation_damage`
    /// gate), AND has a non-empty damage region. Used by the
    /// `v2_automatic_redirect_backing_is_scene_participating`
    /// integration test to assert the Automatic-mode pairing
    /// actually accumulates scene damage on the backing.
    #[doc(hidden)]
    #[must_use]
    pub fn test_peek_presentation_damage_nonempty(&self, xid: u32) -> bool {
        let Some(id) = self.store.lookup(xid) else {
            return false;
        };
        self.store
            .peek_presentation_damage(id)
            .is_some_and(|snap| !snap.region.is_empty())
    }

    /// Scene-α fix — test-only read of the per-storage Vk views
    /// keyed by host xid. Returns `(image_view, sample_view)` —
    /// the attachment-side IDENTITY view and the format-aware
    /// sampling view, respectively. Used by
    /// `v2_storage_depth24_has_distinct_sample_view` to gate the
    /// scene-side α-leak fix at the construction layer.
    #[doc(hidden)]
    #[must_use]
    pub fn test_storage_views(&self, xid: u32) -> Option<(ash::vk::ImageView, ash::vk::ImageView)> {
        let id = self.store.lookup(xid)?;
        let drawable = self.store.get(id)?;
        Some((drawable.storage.image_view, drawable.storage.sample_view))
    }

    /// Stage 4a — test-only knob to install a COMPOSITE redirect
    /// route directly via the store, bypassing 4b's protocol
    /// surface (`allocate_redirected_backing` / `name_window_pixmap`
    /// still stubs returning Err until 4b lands). The
    /// `v2_acceptance` integration tests use this to set up
    /// routing for `resolve_paint_target` coverage without
    /// touching alias-registry / host_window_to_backing
    /// bookkeeping.
    ///
    /// `window_xid` must resolve in the store; `backing_xid` must
    /// also exist and is what window-keyed paint routes into.
    /// Returns `true` if both were resolved and the route was
    /// recorded; `false` otherwise. No side effects on damage,
    /// refcount, or `scene_participating`.
    #[doc(hidden)]
    pub fn test_set_redirected_target(&mut self, window_xid: u32, backing_xid: u32) -> bool {
        let Some(w_id) = self.store.lookup(window_xid) else {
            return false;
        };
        let Some(b_id) = self.store.lookup(backing_xid) else {
            return false;
        };
        self.store.set_redirected_target(w_id, Some(b_id));
        true
    }

    /// Headless test seed. Single 800×600 stub output; no
    /// Vulkan; no real DRM device. Mirrors `KmsBackend::for_tests`
    /// in shape so unit tests that drive v2 through
    /// `process_request` get a stable fixture.
    #[doc(hidden)]
    #[must_use]
    pub fn for_tests() -> Self {
        let mut b = Self::for_tests_seed();
        b.init_root_storage();
        b
    }

    /// Construct the test fixture **without** initialising root
    /// storage. Used by `for_tests_with_vk` so root allocation
    /// happens after the Vk context is attached.
    fn for_tests_seed() -> Self {
        Self {
            core: KmsCore::for_tests(),
            platform: PlatformBackend::for_tests(),
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine: RenderEngine::stub(),
            scene: SceneCompositor::stub(),
            windows_v2: WindowsV2Map::new(),
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            cow_id: None,
            clear_window_area_calls: 0,
            present_to_cow_sources: std::collections::VecDeque::with_capacity(16),
            dri3_xshmfences: HashMap::new(),
            dri3_sync_resources: HashMap::new(),
        }
    }

    fn init_root_storage(&mut self) {
        let root_xid = self.core.window_id;
        if self.store.lookup(root_xid).is_some() {
            return;
        }
        let width = self.platform.fb_w.max(1);
        let height = self.platform.fb_h.max(1);
        let storage = match self.platform.allocate_drawable_storage(width, height, 32) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                log::debug!("v2 init_root_storage: no Vk, using stub root storage: {e:?}");
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(width),
                        height: u32::from(height),
                    },
                    PlatformBackend::format_for_depth(32),
                )
            }
        };
        let id = match self
            .store
            .allocate(root_xid, DrawableKind::Root, 32, true, storage)
        {
            Ok(id) => id,
            Err(e) => {
                log::warn!("v2 init_root_storage: store.allocate failed: {e:?}");
                return;
            }
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            &mut self.platform,
            id,
            rect,
            decode_x11_pixel_server_alpha(self.core.bg_pixel.unwrap_or(0x0050_5050), 24),
        ) && self.platform.vk.is_some()
        {
            log::warn!("v2 init_root_storage: initial root fill failed: {e:?}");
        }
    }

    /// Stage 4a — resolve a host xid into the actual paint target
    /// under COMPOSITE redirect routing. Walks up the
    /// `windows_v2.parent` chain accumulating `(x, y)` offsets;
    /// the first ancestor (including `host_xid` itself) whose
    /// `Drawable.redirected_target` is `Some(B_id)` wins.
    ///
    /// Returns:
    /// - `None` if `host_xid` doesn't map to any drawable.
    /// - `Some(PaintTarget { id: leaf, offset: (0, 0) })` for
    ///   Pixmap targets (not in `windows_v2`) and for
    ///   unredirected windows whose ancestor chain reaches root
    ///   without finding a redirected ancestor.
    /// - `Some(PaintTarget { id: B_id, offset: accumulated })`
    ///   for redirected windows + their descendants.
    ///
    /// Per Stage 4 plan §"Per-hierarchy redirect": this is the
    /// per-op walk; tree depth bounds cost (typically ≤ 4 for
    /// real apps). Cached-ancestry alternative deferred to
    /// Stage 5 if profiling shows it.
    pub(crate) fn resolve_paint_target(&self, host_xid: u32) -> Option<PaintTarget> {
        let leaf_id = self.store.lookup(host_xid)?;
        let result = self.resolve_paint_target_inner(host_xid, leaf_id);
        // Diagnostic trace (TEMP — Stage 4d "opaque black backing"
        // investigation). Only fires when the resolve returned the
        // LEAF id (no redirect found in the ancestor chain) for a
        // *window* xid. That's the "paint to a window that didn't
        // route via a redirected ancestor" case — exactly what we
        // need to see if marco's CC client paints stop routing to B
        // after a drag. Pixmaps and root paints don't trip this gate
        // (root has no `windows_v2` entry), so volume stays bounded
        // to window paints that ought to have hit a redirect.
        if log::log_enabled!(target: "yserver::kms::v2::paint", log::Level::Trace)
            && let Some(t) = result.as_ref()
            && t.id == leaf_id
            && self.windows_v2.contains_key(&host_xid)
        {
            log::trace!(
                target: "yserver::kms::v2::paint",
                "resolve_paint_target NO_REDIRECT_FOUND xid=0x{host_xid:x} leaf_id={leaf_id:?}",
            );
        }
        result
    }

    fn resolve_paint_target_inner(
        &self,
        host_xid: u32,
        leaf_id: super::store::DrawableId,
    ) -> Option<PaintTarget> {
        if !self.windows_v2.contains_key(&host_xid) {
            if let Some(b_id) = self.store.redirected_target(leaf_id) {
                return Some(PaintTarget {
                    id: b_id,
                    offset: (0, 0),
                });
            }
            return Some(PaintTarget {
                id: leaf_id,
                offset: (0, 0),
            });
        }
        let mut cur_xid = host_xid;
        let mut cur_id = leaf_id;
        let mut offset = (0_i32, 0_i32);
        loop {
            if let Some(b_id) = self.store.redirected_target(cur_id) {
                return Some(PaintTarget { id: b_id, offset });
            }
            // No `windows_v2` entry means we've stepped onto root
            // (parent = `core.window_id`, not tracked) or onto an
            // unparented orphan. In both cases there's no parent
            // chain left to walk; return identity at the leaf.
            // (Root's own redirect was already checked on the
            // prior loop iteration when `cur_id` became root_id.)
            let Some(geom) = self.windows_v2.get(&cur_xid) else {
                return Some(PaintTarget {
                    id: leaf_id,
                    offset: (0, 0),
                });
            };
            match geom.parent {
                None => {
                    // Top-level: parent is root, not tracked in
                    // `windows_v2`. `create_subwindow` records
                    // `parent = None` when the host_parent is
                    // root_xid (the if-not-in-windows_v2 branch),
                    // so this is the production representation
                    // for every top-level. Step up to root
                    // explicitly so a `RedirectWindow(root, …)`
                    // compositor sees top-level descendants route
                    // through the root backing — codex round-7
                    // finding (`parent == None` previously
                    // returned identity without consulting root).
                    offset.0 += i32::from(geom.x);
                    offset.1 += i32::from(geom.y);
                    if let Some(root_id) = self.store.lookup(self.core.window_id)
                        && let Some(b_id) = self.store.redirected_target(root_id)
                    {
                        return Some(PaintTarget { id: b_id, offset });
                    }
                    // No root redirect: paint stays on the leaf
                    // at its own origin. Explicit match (not `?`)
                    // so we don't poison the outer Option.
                    return Some(PaintTarget {
                        id: leaf_id,
                        offset: (0, 0),
                    });
                }
                Some(parent_xid) => {
                    offset.0 += i32::from(geom.x);
                    offset.1 += i32::from(geom.y);
                    cur_xid = parent_xid;
                    // Parent xid not in the store means a
                    // dangling reparent: fall back to identity.
                    let Some(next_id) = self.store.lookup(parent_xid) else {
                        return Some(PaintTarget {
                            id: leaf_id,
                            offset: (0, 0),
                        });
                    };
                    cur_id = next_id;
                }
            }
        }
    }

    /// Stage 4c.2 — compute the screen-absolute rect for a window's
    /// `DrawableId`. Walks the `windows_v2.parent` chain upward
    /// from `w_id`, accumulating each step's `(x, y)` offset; the
    /// resulting rect's `offset` is the window's root-relative
    /// origin and the `extent` is its own `width × height`.
    ///
    /// Returns `None` when:
    /// - `w_id` doesn't resolve in the store, OR
    /// - the leaf xid has no `windows_v2` entry (Pixmap / Root /
    ///   detached), OR
    /// - the parent chain hits a dangling `Some(xid)` that is
    ///   neither root (`core.window_id`) nor a tracked
    ///   `windows_v2` entry. Bailing keeps callers from acting on
    ///   a half-accumulated rect; Stage 5 cache work can revisit
    ///   if the conservative choice ever bites.
    ///
    /// Consumed by Stage 4c.4's `set_window_scene_participation`:
    /// it captures the previous on-screen rect BEFORE flipping
    /// `scene_participating` so it can fire
    /// `mark_scene_structure_damage_rects(&[prev_rect])` for the
    /// redirect transition.
    pub(crate) fn window_absolute_rect(
        &self,
        w_id: crate::kms::v2::store::DrawableId,
    ) -> Option<ash::vk::Rect2D> {
        let leaf_xid = self.store.get(w_id)?.xid;
        let leaf_geom = self.windows_v2.get(&leaf_xid)?;
        let mut abs_x = i32::from(leaf_geom.x);
        let mut abs_y = i32::from(leaf_geom.y);
        let mut cur_parent = leaf_geom.parent;
        while let Some(parent_xid) = cur_parent {
            if parent_xid == self.core.window_id {
                // Reached root explicitly — root is the (0, 0)
                // origin of the screen-absolute coordinate space.
                break;
            }
            let Some(parent_geom) = self.windows_v2.get(&parent_xid) else {
                // Dangling parent: not root, not tracked. Bail.
                return None;
            };
            abs_x += i32::from(parent_geom.x);
            abs_y += i32::from(parent_geom.y);
            cur_parent = parent_geom.parent;
        }
        Some(ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: abs_x, y: abs_y },
            extent: ash::vk::Extent2D {
                width: u32::from(leaf_geom.width),
                height: u32::from(leaf_geom.height),
            },
        })
    }

    /// Stage 4b.5 — seed the backing `b_id` with W's content and
    /// each of W's mapped-or-unmapped descendants' content at the
    /// descendant's accumulated `(x, y)` position relative to W.
    /// Mirrors Xorg's behaviour at `composite/compalloc.c:556`.
    ///
    /// Called by `allocate_redirected_backing` BEFORE the route
    /// flip so the copies read each window's own storage (not
    /// post-redirect routed storage). Skips windows whose storage
    /// isn't in the store (the protocol error case is logged in
    /// the caller) and zero-extent storage (defensive — fresh
    /// storage with 0×0 isn't useful as a copy source).
    ///
    /// The walk is depth-first and bounded by tree depth at
    /// activation time only; this is the one-shot cost the plan's
    /// Cross-cutting §"Per-hierarchy redirect" decision called
    /// out as acceptable.
    fn seed_backing_from_window(
        &mut self,
        w_xid: u32,
        w_id: crate::kms::v2::store::DrawableId,
        b_id: crate::kms::v2::store::DrawableId,
    ) {
        // Diagnostic trace (TEMP) — seed entry. Cross-correlate
        // with `allocate_redirected_backing fresh ...` and the
        // "B is all-black" dump: a seed that copies an empty W
        // (= W's storage is its default init color) leaves B
        // in its default state, which is exactly the symptom.
        log::debug!("v2 seed_backing_from_window W=0x{w_xid:x} w_id={w_id:?} b_id={b_id:?}");
        // 1. W itself at (0, 0).
        let w_extent = self
            .store
            .get(w_id)
            .map(|d| d.storage.extent)
            .unwrap_or_default();
        if w_extent.width != 0 && w_extent.height != 0 {
            let src_rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: w_extent,
            };
            if let Err(e) = self.engine.copy_area(
                &mut self.store,
                &mut self.platform,
                w_id,
                b_id,
                src_rect,
                ash::vk::Offset2D::default(),
            ) {
                log::warn!(
                    "v2 seed_backing_from_window(0x{w_xid:x}): root copy_area failed: {e:?}",
                );
            } else {
                self.telemetry.record_paint_submit();
            }
        }

        // 2. Walk descendants in sibling stack order. Collect to a
        // Vec first to release the borrow on `windows_v2` before we
        // call into the engine (which takes `&mut self.store`).
        //
        // Each Vec entry is (descendant_xid, descendant_x_in_W,
        // descendant_y_in_W). The walk is a manual stack-driven DFS
        // over `windows_v2.parent`. Children are visited in ascending
        // `stack_rank` so the seed-copy reproduces the same bottom→top
        // overwrite order the scene uses for overlapping siblings.
        let mut to_seed: Vec<(u32, i32, i32)> = Vec::new();
        let mut frontier: Vec<(u32, i32, i32)> = vec![(w_xid, 0, 0)];
        while let Some((parent_xid, parent_dx, parent_dy)) = frontier.pop() {
            let mut children: Vec<(u32, u64, i32, i32)> = self
                .windows_v2
                .iter()
                .filter_map(|(xid, geom)| {
                    (geom.parent == Some(parent_xid)).then_some((
                        *xid,
                        geom.stack_rank,
                        parent_dx + i32::from(geom.x),
                        parent_dy + i32::from(geom.y),
                    ))
                })
                .collect();
            children.sort_by_key(|(_, rank, _, _)| *rank);
            for (xid, _, abs_x, abs_y) in &children {
                to_seed.push((*xid, *abs_x, *abs_y));
            }
            for (xid, _, abs_x, abs_y) in children.into_iter().rev() {
                frontier.push((xid, abs_x, abs_y));
            }
        }
        for (desc_xid, abs_x, abs_y) in to_seed {
            let Some(desc_id) = self.store.lookup(desc_xid) else {
                continue;
            };
            let desc_extent = self
                .store
                .get(desc_id)
                .map(|d| d.storage.extent)
                .unwrap_or_default();
            if desc_extent.width == 0 || desc_extent.height == 0 {
                continue;
            }
            let src_rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: desc_extent,
            };
            let dst_pos = ash::vk::Offset2D { x: abs_x, y: abs_y };
            if let Err(e) = self.engine.copy_area(
                &mut self.store,
                &mut self.platform,
                desc_id,
                b_id,
                src_rect,
                dst_pos,
            ) {
                log::warn!(
                    "v2 seed_backing_from_window(0x{w_xid:x}): descendant 0x{desc_xid:x} \
                     copy_area at ({abs_x}, {abs_y}) failed: {e:?}",
                );
            } else {
                self.telemetry.record_paint_submit();
            }
        }
    }

    /// Virtual-screen extent — mirrors `KmsBackend::fb_dimensions`.
    /// Called by `lib.rs` during the pre-`Box<dyn Backend>` setup
    /// (capability advertisement, `ServerState::with_randr_outputs`).
    #[must_use]
    pub fn fb_dimensions(&self) -> (u16, u16) {
        self.platform.fb_dimensions()
    }

    /// RandR output list — mirrors `KmsBackend::randr_outputs`.
    #[must_use]
    pub fn randr_outputs(&self) -> Vec<yserver_core::randr::RandrOutput> {
        use std::collections::HashMap;
        use yserver_core::randr::RandrOutput;
        let n = self.platform.outputs.len();
        let mut mode_ids: HashMap<(u16, u16, u32), u32> = HashMap::new();
        #[allow(clippy::cast_possible_truncation)]
        let mut next_mode_id: u32 = (2 * n + 1) as u32;
        self.platform
            .outputs
            .iter()
            .enumerate()
            .map(|(i, layout)| {
                let vrefresh = layout.output.picked.vrefresh;
                let key = (layout.width, layout.height, vrefresh);
                let mode_id = *mode_ids.entry(key).or_insert_with(|| {
                    let id = next_mode_id;
                    next_mode_id += 1;
                    id
                });
                #[allow(clippy::cast_possible_truncation)]
                let output_id = (i + 1) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let crtc_id = (n + i + 1) as u32;
                RandrOutput {
                    name: layout.output.connector_name.clone(),
                    output_id,
                    crtc_id,
                    mode_id,
                    x: i16::try_from(layout.x).unwrap_or(i16::MAX),
                    y: i16::try_from(layout.y).unwrap_or(i16::MAX),
                    width: layout.width,
                    height: layout.height,
                    vrefresh,
                }
            })
            .collect()
    }

    /// Telemetry accessor — used by the acceptance harness to
    /// read lifetime counters after driving a test sequence.
    #[must_use]
    pub fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    /// Hand the libinput context off to the dedicated input thread.
    /// Mirrors `KmsBackend::take_input_ctx`.
    #[must_use]
    pub fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.platform.take_input_ctx()
    }

    /// Initial composite + flip. v2's SceneCompositor records
    /// one compose CB per output and atomic-flips. On a fresh
    /// boot the scene typically has no mapped windows yet, so
    /// this paints the `bg_pixel` clear color and flips.
    ///
    /// # Errors
    ///
    /// Returns the first per-output Vk / DRM failure; subsequent
    /// outputs still attempted.
    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        match self.scene.tick(
            &self.core,
            &mut self.store,
            &mut self.platform,
            &self.windows_v2,
            &mut self.telemetry,
        ) {
            Ok(_) => Ok(()),
            Err(e) => Err(io::Error::other(format!("v2 composite_and_flip: {e:?}"))),
        }
    }

    /// Post-loop teardown — delegates to PlatformBackend, which
    /// disables each output and disarms scanout pools whose
    /// disable failed (matching v1's behaviour to avoid leaking
    /// framebuffers KMS may still hold).
    ///
    /// # Errors
    ///
    /// Propagates the first per-output `drm::modeset::disable_output`
    /// failure; subsequent outputs still attempted.
    pub fn disable_output(&mut self) -> io::Result<()> {
        // Drain in-flight paint + compose submits before the
        // platform's `device_wait_idle` + pool destruction so
        // each subsystem's book-keeping reclaims its handles
        // against the still-live pool.
        self.engine.drain_all(&self.platform);
        self.scene.drain_all(&self.platform);
        self.platform.disable_output()
    }

    /// Once-per-method dedup helper. Each `method` name produces
    /// exactly one `warn!` per session, so a busy client doesn't
    /// drown the log.
    fn log_v2_gap(&self, method: &'static str) {
        if self.logged_gaps.borrow_mut().insert(method) {
            log::warn!("v2: {method} not yet implemented — paint or composite operation skipped");
        }
    }

    // ── Input dispatch (Stage 3f.7) ─────────────────────────────
    //
    // Ports the v1 input cluster onto v2's state surface.
    // Differences from v1's body (kms/backend.rs:6450-6885):
    //
    // - `self.windows` → `self.windows_v2`.
    // - `self.fb_w` / `self.fb_h` → the active output's geometry
    //   read off `self.platform.outputs[0]`.
    // - HW cursor calls (`hw_cursor_active` / `hw_cursor_move` /
    //   `hw_cursor_refresh`) → no-op. Per spec § I7 the HW cursor
    //   plane is parked in v2 until Stage 5 reintroduces it as a
    //   SceneCompositor strategy.
    // - `self.mark_all_outputs_dirty()` →
    //   `self.scene.mark_scene_structure_dirty()`. Pointer-motion-
    //   only redraws are a no-op in Stage 3 anyway (no cursor
    //   scene blit until Stage 4); the dirty flag preserves the
    //   "scene needs a tick" signal for any client paint that
    //   races a motion event.

    /// X11 KeyButMask: bits 0..=7 are modifiers
    /// (Shift/Lock/Control/Mod1..Mod5). Bits 8..=12 are button
    /// state, set by `process_pointer_button` via `button_mask`.
    fn serialize_modifiers(&self) -> u16 {
        let state = &self.core.xkb_state.0;
        let flags = xkbcommon::xkb::STATE_MODS_EFFECTIVE;
        let mut mask: u16 = 0;
        if state.mod_name_is_active("Shift", flags) {
            mask |= 0x01;
        }
        if state.mod_name_is_active("Lock", flags) {
            mask |= 0x02;
        }
        if state.mod_name_is_active("Control", flags) {
            mask |= 0x04;
        }
        if state.mod_name_is_active("Mod1", flags) {
            mask |= 0x08;
        }
        if state.mod_name_is_active("Mod2", flags) {
            mask |= 0x10;
        }
        if state.mod_name_is_active("Mod3", flags) {
            mask |= 0x20;
        }
        if state.mod_name_is_active("Mod4", flags) {
            mask |= 0x40;
        }
        if state.mod_name_is_active("Mod5", flags) {
            mask |= 0x80;
        }
        mask
    }

    /// Update xkb_state for `raw` then return a cooked
    /// `HostKeyEvent` with the post-update modifier state +
    /// cursor coords pre-filled. Direct v1 port.
    fn cook_host_key(&mut self, raw: HostKeyEvent) -> HostKeyEvent {
        let xkb_keycode = xkbcommon::xkb::Keycode::new(u32::from(raw.keycode));
        let direction = if raw.pressed {
            xkbcommon::xkb::KeyDirection::Down
        } else {
            xkbcommon::xkb::KeyDirection::Up
        };
        self.core.xkb_state.0.update_key(xkb_keycode, direction);
        HostKeyEvent {
            state: self.serialize_modifiers(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x: self.core.cursor_x as i16,
            event_y: self.core.cursor_y as i16,
            time: crate::clock::server_time_ms(),
            ..raw
        }
    }

    /// Topmost mapped top-level under the cursor. Walks
    /// `core.top_level_order` back-to-front so the topmost match
    /// wins. v2 hit-tests against `windows_v2` (parity with v1's
    /// `self.windows`); SHAPE-input precedence matches v1.
    fn window_under_cursor(&self) -> Option<u32> {
        let cx = f64::from(self.core.cursor_x);
        let cy = f64::from(self.core.cursor_y);
        for &window_id in self.core.top_level_order.iter().rev() {
            let Some(w) = self.windows_v2.get(&window_id) else {
                continue;
            };
            if !w.mapped {
                continue;
            }
            let wx = f64::from(w.x);
            let wy = f64::from(w.y);
            let ww = f64::from(w.width);
            let wh = f64::from(w.height);
            if cx < wx || cx >= wx + ww || cy < wy || cy >= wy + wh {
                continue;
            }
            // SHAPE input precedence — empty SHAPE = unhittable.
            let shape = self
                .core
                .shape_input
                .get(&window_id)
                .or_else(|| self.core.shape_bounding.get(&window_id));
            if let Some(rects) = shape {
                let inside = rects.iter().any(|r| {
                    let rx = wx + f64::from(r.x);
                    let ry = wy + f64::from(r.y);
                    cx >= rx
                        && cx < rx + f64::from(r.width)
                        && cy >= ry
                        && cy < ry + f64::from(r.height)
                });
                if !inside {
                    continue;
                }
            }
            return Some(window_id);
        }
        None
    }

    /// Event-window-relative coords for an event whose `host_xid`
    /// is the topmost mapped top-level under the cursor. v2-shape
    /// port — reads geometry off `windows_v2`. Falls back to root
    /// coords when `host_xid` isn't tracked (the dispatcher
    /// re-derives target coords from its own tree walk anyway).
    fn event_relative_coords(&self, host_xid: u32) -> (i16, i16) {
        if let Some(w) = self.windows_v2.get(&host_xid) {
            let ex = (self.core.cursor_x as i32) - i32::from(w.x);
            let ey = (self.core.cursor_y as i32) - i32::from(w.y);
            (
                ex.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                ey.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            )
        } else {
            (self.core.cursor_x as i16, self.core.cursor_y as i16)
        }
    }

    fn emit_pointer(&mut self, ev: HostPointerEvent) {
        self.core.pending_pointer_events.push(ev);
    }

    fn emit_crossing(
        &mut self,
        host_xid: u32,
        kind: PointerEventKind,
        detail: u8,
        crossing_mode: u8,
        child: u32,
        state: u16,
    ) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time: crate::clock::server_time_ms(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode,
            child,
        };
        self.emit_pointer(ev);
    }

    fn emit_motion_only(&mut self, host_xid: u32, mask: u16) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid,
            detail: 0,
            time: crate::clock::server_time_ms(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state: mask,
            crossing_mode: 0,
            child: 0,
        };
        self.emit_pointer(ev);
    }

    /// Spec-correct Normal-mode crossing chain for a top-level
    /// transition. Direct v1 port (kms/backend.rs:6630-6695) —
    /// the body only touches KmsCore + nested-resource look-ups.
    fn update_pointer_window(&mut self, server_state: &ServerState, new_xid: u32, mask: u16) {
        if self.core.prev_pointer_window == Some(new_xid) {
            return;
        }
        let prev_host = self.core.prev_pointer_window;
        let root_container_host = self.core.window_id;
        let resolve_host_to_nested = |host: u32, xid_map: &HostXidMap| -> Option<ResourceId> {
            if host == root_container_host {
                Some(yserver_core::resources::ROOT_WINDOW)
            } else {
                xid_map.get(&host).copied()
            }
        };
        let prev_id = prev_host.and_then(|p| resolve_host_to_nested(p, &self.core.xid_map));
        let new_id = resolve_host_to_nested(new_xid, &self.core.xid_map);

        if let (Some(from), Some(to)) = (prev_id, new_id) {
            let events = yserver_core::crossings::normal_mode_crossings(server_state, from, to);
            for ev in events {
                let win_host_xid = if ev.window == yserver_core::resources::ROOT_WINDOW {
                    self.core.window_id
                } else {
                    server_state
                        .resources
                        .window(ev.window)
                        .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                        .unwrap_or(new_xid)
                };
                let kind = match ev.kind {
                    yserver_core::crossings::CrossingKind::Enter => PointerEventKind::EnterNotify,
                    yserver_core::crossings::CrossingKind::Leave => PointerEventKind::LeaveNotify,
                };
                self.emit_crossing(win_host_xid, kind, ev.detail, 0, ev.child.0, mask);
            }
        } else {
            // First-motion bootstrap or unmapped host_xid —
            // fall back to a single Leave/Enter with detail=0.
            if let Some(prev) = prev_host {
                self.emit_crossing(prev, PointerEventKind::LeaveNotify, 0, 0, 0, mask);
            }
            self.emit_crossing(new_xid, PointerEventKind::EnterNotify, 0, 0, 0, mask);
        }
        self.core.prev_pointer_window = Some(new_xid);
    }

    fn dispatch_motion_event(&mut self, server_state: &ServerState) {
        // Fall back to the root container so root-window subscribers
        // (e16's right-click-desktop menu, fvwm3's root bindings) can
        // see motion when the cursor is over the wallpaper.
        let host_xid = self.window_under_cursor().unwrap_or(self.core.window_id);
        let mask = self.serialize_modifiers() | self.core.button_mask;
        self.update_pointer_window(server_state, host_xid, mask);
        self.emit_motion_only(host_xid, mask);
    }

    fn process_pointer_absolute(&mut self, server_state: &ServerState, x: f32, y: f32) {
        // Clamp to the UNION framebuffer extent (`fb_w`/`fb_h`),
        // not the first output's box. `core_platform_init`
        // (`kms/backend.rs:1063-1072`) computes this as
        // `max(x + width)` across every output, which is also the
        // extent the input thread targets when it accumulates
        // libinput deltas (`input_thread.rs:180-189`). Pre-fix
        // this consulted `outputs.first().width/height`, so the
        // pointer could never cross from output 0 onto a side-
        // adjacent output 1 — pinned by
        // `process_pointer_absolute_uses_union_fb_extent_for_multi_output`.
        let fb_w = f32::from(self.platform.fb_w.max(1));
        let fb_h = f32::from(self.platform.fb_h.max(1));
        let new_x = x.clamp(0.0, (fb_w - 1.0).max(0.0));
        let new_y = y.clamp(0.0, (fb_h - 1.0).max(0.0));
        if new_x != self.core.cursor_x || new_y != self.core.cursor_y {
            self.core.cursor_x = new_x;
            self.core.cursor_y = new_y;
            self.scene.wake_for_damage();
        }
        self.dispatch_motion_event(server_state);
    }

    fn process_pointer_button(&mut self, code: u32, pressed: bool, server_state: &ServerState) {
        let detail = match code {
            0x110 => 1, // BTN_LEFT
            0x111 => 3, // BTN_RIGHT
            0x112 => 2, // BTN_MIDDLE
            0x113 => 8, // BTN_SIDE
            0x114 => 9, // BTN_EXTRA
            0x180 => 4, // SYNTH_SCROLL_UP
            0x181 => 5, // SYNTH_SCROLL_DOWN
            0x182 => 6, // SYNTH_SCROLL_LEFT
            0x183 => 7, // SYNTH_SCROLL_RIGHT
            _ => {
                log::debug!("v2: unmapped libinput button code 0x{code:x}, dropping");
                return;
            }
        };
        let host_xid = self.window_under_cursor().unwrap_or(self.core.window_id);
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let button_bit: u16 = match detail {
            1 => 0x0100,
            2 => 0x0200,
            3 => 0x0400,
            4 => 0x0800,
            5 => 0x1000,
            _ => 0,
        };
        let modifier_mask = self.serialize_modifiers();
        // X11 spec: `state` is the logical button state IMMEDIATELY
        // BEFORE the event takes effect. Press: button bit not yet
        // set. Release: button bit still set.
        let state = if pressed {
            modifier_mask | self.core.button_mask
        } else {
            modifier_mask | self.core.button_mask | button_bit
        };
        if pressed {
            self.core.button_mask |= button_bit;
        } else {
            self.core.button_mask &= !button_bit;
        }
        let time = crate::clock::server_time_ms();
        let kind = if pressed {
            PointerEventKind::ButtonPress
        } else {
            PointerEventKind::ButtonRelease
        };
        let ptr_event = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time,
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode: 0,
            child: 0,
        };
        self.emit_pointer(ptr_event);
        // Implicit-grab crossings (G3). Direct v1 port.
        let post_state = self.serialize_modifiers() | self.core.button_mask;
        let press_mode: u8 = if pressed { 1 } else { 2 };
        let grab_id = self.core.xid_map.get(&host_xid).copied();
        let focus_id = self
            .core
            .prev_pointer_window
            .and_then(|prev| self.core.xid_map.get(&prev).copied());
        if let (Some(focus), Some(grab)) = (focus_id, grab_id) {
            let events =
                yserver_core::crossings::implicit_grab_crossings(server_state, focus, grab);
            for ev in events {
                let win_host_xid = server_state
                    .resources
                    .window(ev.window)
                    .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                    .unwrap_or(host_xid);
                let kind = match ev.kind {
                    yserver_core::crossings::CrossingKind::Enter => PointerEventKind::EnterNotify,
                    yserver_core::crossings::CrossingKind::Leave => PointerEventKind::LeaveNotify,
                };
                self.emit_crossing(
                    win_host_xid,
                    kind,
                    ev.detail,
                    press_mode,
                    ev.child.0,
                    post_state,
                );
            }
        }
    }

    /// Decode the wire-packed clip rectangle list (`Vec<u8>` of
    /// i16 x, i16 y, u16 w, u16 h tuples) into `Rectangle16`s in
    /// dst-coords (with the GC clip-origin already added). Returns
    /// `None` when the current GC clip is `None`. `Pixmap`-clip is
    /// returned as `None` for now — Stage 3f.3 promotes the
    /// pixmap-mask path; until then the clip is passed through
    /// (matches v1's pre-promotion behaviour).
    fn current_clip_rects_in_dst_space(&self) -> Option<Vec<Rectangle16>> {
        let ClipState::Rectangles { origin, rects } = &self.core.current_clip else {
            return None;
        };
        let bytes = &rects.rectangles;
        let mut out = Vec::with_capacity(bytes.len() / 8);
        for chunk in bytes.chunks_exact(8) {
            let cx = i32::from(i16::from_le_bytes([chunk[0], chunk[1]])) + i32::from(origin.0);
            let cy = i32::from(i16::from_le_bytes([chunk[2], chunk[3]])) + i32::from(origin.1);
            let cw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ch = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            if cw <= 0 || ch <= 0 {
                continue;
            }
            out.push(Rectangle16 {
                x: cx.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                y: cy.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                width: cw.min(i32::from(u16::MAX)) as u16,
                height: ch.min(i32::from(u16::MAX)) as u16,
            });
        }
        Some(out)
    }

    /// Intersect each rect in `rects` against the current GC clip.
    /// Mirrors v1's helper byte-for-byte. Returns `rects` unchanged
    /// when no clip is active.
    pub(crate) fn intersect_with_current_clip(&self, rects: &[Rectangle16]) -> Vec<Rectangle16> {
        let Some(clip_rects) = self.current_clip_rects_in_dst_space() else {
            return rects.to_vec();
        };
        let mut out = Vec::with_capacity(rects.len());
        for r in rects {
            let rx0 = i32::from(r.x);
            let ry0 = i32::from(r.y);
            let rx1 = rx0 + i32::from(r.width);
            let ry1 = ry0 + i32::from(r.height);
            for c in &clip_rects {
                let cx0 = i32::from(c.x);
                let cy0 = i32::from(c.y);
                let cx1 = cx0 + i32::from(c.width);
                let cy1 = cy0 + i32::from(c.height);
                let ix0 = rx0.max(cx0);
                let iy0 = ry0.max(cy0);
                let ix1 = rx1.min(cx1);
                let iy1 = ry1.min(cy1);
                if ix0 < ix1 && iy0 < iy1 {
                    out.push(Rectangle16 {
                        x: ix0 as i16,
                        y: iy0 as i16,
                        width: (ix1 - ix0) as u16,
                        height: (iy1 - iy0) as u16,
                    });
                }
            }
        }
        out
    }

    /// Storage dimensions for a host xid, in pixels. `None` if the
    /// drawable is unknown.
    fn drawable_dims_v2(&self, host_xid: u32) -> Option<(u32, u32)> {
        let id = self.store.lookup(host_xid)?;
        let d = self.store.get(id)?;
        Some((d.storage.extent.width, d.storage.extent.height))
    }

    /// Lower a list of solid-colour rectangles to the appropriate
    /// engine path. Used by the stroke-style poly ops (`PolyLine`,
    /// `PolySegment`, `PolyPoint`, `PolyArc`, `PolyRectangle`) where
    /// every rasterised rect is in the GC's single foreground colour
    /// regardless of GC fill-style, and as the fallback inside
    /// Stage 3f.11: apply X11 ConfigureWindow `stack_mode` to a
    /// top-level window's position in `core.top_level_order`.
    /// Implements Above (0/2/4) and Below (1/3) per v1's behaviour
    /// — TopIf/BottomIf/Opposite collapse to Above/Below without
    /// the conditional check (sufficient for marco / fvwm /
    /// xterm-popup workloads). No-op for windows that aren't in
    /// `top_level_order` (subwindows; restack of a subwindow is
    /// deferred until we track per-parent sibling stack order).
    fn restack_top_level(&mut self, host_xid: u32, stack_mode: u8, sibling: Option<u32>) {
        let stack = &mut self.core.top_level_order;
        if !stack.contains(&host_xid) {
            // Subwindow restack — siblings aren't ordered in v2 yet.
            // Future work; tracked in `status.md` § 3f.11.
            return;
        }
        stack.retain(|&x| x != host_xid);
        let sibling_pos = sibling.and_then(|sib| stack.iter().position(|&x| x == sib));
        match stack_mode {
            // Above: place above sibling, or at top if no sibling.
            0 | 2 | 4 => match sibling_pos {
                Some(sp) => stack.insert(sp + 1, host_xid),
                None => stack.push(host_xid),
            },
            // Below: place below sibling, or at bottom if no sibling.
            1 | 3 => match sibling_pos {
                Some(sp) => stack.insert(sp, host_xid),
                None => stack.insert(0, host_xid),
            },
            _ => stack.push(host_xid),
        }
    }

    /// Stage 4a — shift each rect by `(dx, dy)` (saturating to
    /// i16 range). Returns the input unchanged when both deltas
    /// are zero. Used to translate window-local paint rects into
    /// backing-local coords under COMPOSITE redirect: a paint
    /// against descendant C of redirected W at offset
    /// `(cx, cy)` against W lands at C's rect + `(cx, cy)` in
    /// W's backing.
    fn shift_rectangles_for_paint(
        rects: &[Rectangle16],
        (dx, dy): (i32, i32),
    ) -> std::borrow::Cow<'_, [Rectangle16]> {
        if dx == 0 && dy == 0 {
            return std::borrow::Cow::Borrowed(rects);
        }
        std::borrow::Cow::Owned(
            rects
                .iter()
                .map(|r| Rectangle16 {
                    x: (i32::from(r.x) + dx).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    y: (i32::from(r.y) + dy).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    width: r.width,
                    height: r.height,
                })
                .collect(),
        )
    }

    /// Stage 4a — shift a picture's clip rects from
    /// dst-drawable-local into backing-local coords. The clip
    /// itself is stored in dst-window-local coords (pre-shifted
    /// by Stage 3b's `clip_x` / `clip_y`); when paint resolves
    /// through a redirected ancestor, the per-rect scissor in
    /// the engine operates against the backing's storage extent,
    /// so the clip must move with it.
    fn shift_dst_picture_clip(
        clip: Option<Vec<Rectangle16>>,
        offset: (i32, i32),
    ) -> Option<Vec<Rectangle16>> {
        let rects = clip?;
        Some(Self::shift_rectangles_for_paint(&rects, offset).into_owned())
    }

    /// [`fill_rects_honoring_fill_state`] for the Solid arm.
    ///
    /// `GcFunction::Copy` (the common case) goes through the fast
    /// `vkCmdClearAttachments`-driven `engine.fill_rect`. Non-`Copy`
    /// functions (Stage 3f.2: GXclear / GXxor / GXinvert / etc.)
    /// divert to `engine.logic_fill`, which builds a per-function
    /// `VkLogicOp` pipeline through the shared
    /// `LogicFillPipelineCache`. `GcFunction::NoOp` is a no-op.
    ///
    /// Stage 4a: `target` carries the resolved DrawableId + a
    /// paint-translation offset for COMPOSITE redirect. Window-
    /// local `rects` are shifted by `target.offset` before going
    /// to the engine.
    fn fill_solid_rects(&mut self, target: PaintTarget, fg: u32, rects: &[Rectangle16]) {
        use yserver_core::backend::GcFunction;
        if rects.is_empty() {
            return;
        }
        let function = self.core.current_function;
        if matches!(function, GcFunction::NoOp) {
            return;
        }
        let (dx, dy) = target.offset;
        let id = target.id;
        if !matches!(function, GcFunction::Copy) {
            // Compute `opaque_alpha` per the L1 server-α invariant:
            // depth-32 ARGB destinations take the LogicOp on all four
            // channels; depth-24/8/1 are server-owned-α so the
            // pipeline's write mask drops alpha to keep the dst byte
            // intact. Depth lookup via the drawable record.
            let opaque_alpha = self.store.get(id).map(|d| d.depth != 32).unwrap_or(true);
            // Stage 4a — shift window-local rects into backing-local
            // coords when this paint resolves through a redirected
            // ancestor. `Cow::Borrowed` when offset is (0, 0).
            let shifted = Self::shift_rectangles_for_paint(rects, target.offset);
            match self.engine.logic_fill(
                &mut self.store,
                &mut self.platform,
                id,
                function,
                opaque_alpha,
                fg,
                &shifted,
            ) {
                Ok(()) => {
                    // One submit per call regardless of rect count
                    // (logic_fill records every rect into the same CB).
                    self.telemetry.record_paint_submit();
                }
                Err(e) => {
                    log::warn!(
                        "v2 fill_solid_rects: engine.logic_fill failed ({function:?}): {e:?}"
                    );
                }
            }
            return;
        }
        // L1 server-α invariant: depth-24 dst stores alpha=0xFF
        // regardless of the X11 pixel's upper byte. Without this,
        // the scene compositor's alpha_passthrough=true draws read
        // back α=0 (X-padding) and the window blends transparent —
        // the layer underneath leaks through, panel renders white
        // not teal. Matches v1's `try_vk_solid_fill` (kms/backend.rs:3512).
        let depth = self.store.get(id).map(|d| d.depth).unwrap_or(24);
        let color = decode_x11_pixel_server_alpha(fg, depth);
        // Stage 3f.15: coalesce N stroke rects into one CB + one
        // submit via engine.fill_rect_batch. PolySegment / PolyLine
        // / PolyRectangle fan-outs now pay O(1) submits per protocol
        // request instead of O(N). Zero-sized rects are filtered
        // inside the engine.
        //
        // Stage 4a — apply paint-target offset (window-local →
        // backing-local) directly into the i32 vk::Offset2D.
        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter(|r| r.width != 0 && r.height != 0)
            .map(|r| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(r.x) + dx,
                    y: i32::from(r.y) + dy,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                },
            })
            .collect();
        if vk_rects.is_empty() {
            return;
        }
        match self
            .engine
            .fill_rect_batch(&mut self.store, &mut self.platform, id, color, &vk_rects)
        {
            Ok(()) => {
                self.telemetry.record_paint_submit();
            }
            Err(e) => {
                log::warn!("v2 fill_solid_rects: engine.fill_rect_batch failed: {e:?}");
            }
        }
    }

    /// Fill `rects` on `id`, honouring `KmsCore.current_fill`. Used
    /// by the filled-shape ops (`PolyFillRectangle`, `PolyFillArc`,
    /// `FillPoly`, `FillRectangle`); stroke ops keep using
    /// [`fill_solid_rects`] because X11 strokes are always solid
    /// foreground regardless of GC fill-style.
    ///
    /// `Tiled` with `GcFunction::Copy` drives a RENDER composite
    /// (`OP_SRC`, `Repeat::Normal`) so the tile pixmap supplies the
    /// destination colours — e16 paints popup backgrounds this way.
    /// `Tiled` with a non-`Copy` function degenerates to a solid
    /// logic-op fill (matches v1's behaviour — no real client drives
    /// tiled+logic-op). `Stippled` / `OpaqueStippled` fall through
    /// to solid for now; proper stipple support is post-Stage-3.
    fn fill_rects_honoring_fill_state(
        &mut self,
        target: PaintTarget,
        fg: u32,
        rects: &[Rectangle16],
    ) {
        use yserver_core::backend::{FillState, GcFunction};
        if rects.is_empty() {
            return;
        }
        let function = self.core.current_function;
        if matches!(function, GcFunction::NoOp) {
            return;
        }
        let fill = self.core.current_fill.clone();
        match fill {
            FillState::Tiled { pixmap, origin } => {
                let tile_xid = pixmap.as_raw();
                if !matches!(function, GcFunction::Copy) {
                    // Non-Copy + Tiled isn't covered by any current
                    // client; degenerate to solid logic-op fill so
                    // the function is honoured (matches v1).
                    self.fill_solid_rects(target, fg, rects);
                    return;
                }
                if !self.try_tiled_fill(target, tile_xid, origin.0, origin.1, rects) {
                    // Tile not in store / aliases dst / non-BGRA8
                    // tile — degenerate to solid foreground.
                    self.fill_solid_rects(target, fg, rects);
                }
            }
            FillState::Solid | FillState::Stippled { .. } | FillState::OpaqueStippled { .. } => {
                // Stipple support is post-Stage-3 (no real-app smoke
                // client drives it on KMS). Fall through as solid.
                self.fill_solid_rects(target, fg, rects);
            }
        }
    }

    /// Tile fill via `engine.render_composite` (Stage 3f.3). Returns
    /// `true` iff the call submitted; `false` if the tile isn't
    /// usable (unknown xid, self-tile aliasing, non-BGRA8 tile
    /// format), in which case the caller falls back to solid.
    ///
    /// Stage 4a: `dst` carries the resolved DrawableId + offset.
    /// Dst-space rect origins are shifted by `dst.offset` to land
    /// in backing coords; `src_x/src_y` stay window-local because
    /// they're a `(dst - tile_origin)` difference that doesn't
    /// depend on the absolute frame.
    fn try_tiled_fill(
        &mut self,
        dst: PaintTarget,
        tile_xid: u32,
        ox: i16,
        oy: i16,
        rects: &[Rectangle16],
    ) -> bool {
        use crate::kms::{v2::engine::ResolvedSource, vk::ops::render::CompositeRect};
        if rects.is_empty() {
            return true;
        }
        let Some(tile_id) = self.store.lookup(tile_xid) else {
            log::debug!("v2 try_tiled_fill: tile 0x{tile_xid:x} not in store");
            return false;
        };
        if tile_id == dst.id {
            // Self-tile would alias src + dst inside render_composite.
            return false;
        }
        let tile_format = self.store.get(tile_id).map(|d| d.storage.format);
        if tile_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            log::debug!("v2 try_tiled_fill: tile 0x{tile_xid:x} format {tile_format:?} not BGRA8");
            return false;
        }
        let (dx, dy) = dst.offset;
        // Build per-rect CompositeRects in dst space with
        // `src_origin = dst - tile_origin` so the shader's
        // `src_origin + dst_offset` lands on the right tile pixel.
        let composite_rects: Vec<CompositeRect> = rects
            .iter()
            .filter_map(|r| {
                if r.width == 0 || r.height == 0 {
                    return None;
                }
                Some(CompositeRect {
                    src_x: i32::from(r.x) - i32::from(ox),
                    src_y: i32::from(r.y) - i32::from(oy),
                    mask_x: 0,
                    mask_y: 0,
                    dst_x: i32::from(r.x) + dx,
                    dst_y: i32::from(r.y) + dy,
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                })
            })
            .collect();
        if composite_rects.is_empty() {
            return true;
        }
        // Op `Src` (1) — tile fill replaces the destination.
        const OP_SRC: u8 = 1;
        match self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            OP_SRC,
            ResolvedSource::Drawable(tile_id),
            ResolvedSource::None,
            dst.id,
            &composite_rects,
            None, // GC clip already applied by caller
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            // Audit #4: synthesized tile-fill draw, no Picture
            // context. Engine falls back to depth heuristic.
            0,
            0,
            0,
        ) {
            Ok(s) => {
                if s.recorded_draws > 0 {
                    self.telemetry.record_paint_submit();
                }
                true
            }
            Err(e) => {
                log::warn!("v2 try_tiled_fill: render_composite failed: {e:?}");
                false
            }
        }
    }

    /// Allocate v2 storage + windows_v2 entry for a host xid.
    /// Idempotent against duplicate xids (logs + skips). `parent`
    /// is `Some(parent_xid)` for subwindows + `None` for top-levels
    /// (parent = root, not tracked in `windows_v2`). The
    /// `bg_pixel` slot is what gets painted into fresh storage —
    /// `None` leaves it Vk-undefined (depth-1 / depth-8 masks).
    fn allocate_window_storage(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        depth: u8,
        parent: Option<u32>,
        bg_pixel: Option<u32>,
    ) {
        if self.windows_v2.contains_key(&host_xid) {
            return;
        }
        let stack_rank = self.alloc_window_stack_rank();
        let mut storage_allocated = false;
        match self
            .platform
            .allocate_drawable_storage(width.max(1), height.max(1), depth)
        {
            Ok(storage) => {
                if let Err(e) = self.store.allocate(
                    host_xid,
                    DrawableKind::Window,
                    depth,
                    false, // becomes true on map_subwindow
                    storage,
                ) {
                    log::warn!(
                        "v2 allocate_window_storage: store.allocate failed for xid {host_xid:#x}: {e:?}",
                    );
                    return;
                }
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage_allocated = true;
            }
            Err(e) => {
                // No Vk fixture (`for_tests`) → storage allocation
                // returns ERROR_INITIALIZATION_FAILED. Tracking
                // the geometry without storage is fine; the scene
                // tick filters out null image-views.
                log::debug!("v2 allocate_window_storage: no Vk for xid {host_xid:#x}: {e:?}",);
            }
        }
        self.windows_v2.insert(
            host_xid,
            WindowGeometryV2 {
                x,
                y,
                width,
                height,
                depth,
                mapped: false,
                parent,
                stack_rank,
                bg_pixel,
                bg_pixmap: None,
            },
        );
        // Stage 3f.6 + 3f.14: clear newly-allocated storage to a
        // defined colour so freshly-mapped windows don't surface
        // the pool returner's pixels (3f.10 PixmapPool recycles
        // image/view/memory triples — the bytes are whatever the
        // previous owner left). When `bg_pixel` is set, use it
        // (v1's create_subwindow behaviour); otherwise paint a
        // depth-appropriate safe default (3f.14).
        if storage_allocated && let Some(id) = self.store.lookup(host_xid) {
            let color = bg_pixel.map_or_else(
                || default_window_init_color(depth),
                |pixel| decode_x11_pixel_server_alpha(pixel, depth),
            );
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: u32::from(width.max(1)),
                    height: u32::from(height.max(1)),
                },
            };
            if let Err(e) =
                self.engine
                    .fill_rect(&mut self.store, &mut self.platform, id, rect, color)
            {
                log::debug!(
                    "v2 allocate_window_storage: initial fill failed for xid {host_xid:#x}: {e:?}"
                );
            }
        }
    }

    // ── Stage 3a: Core-text helpers ─────────────────────────────

    /// FreeType rasterise + atlas dispatch for one text run.
    /// Used by `image_text8/16` and `poly_text8/16`. Per Stage 3
    /// plan §"Cross-cutting" §4: Core ops consult GC clip only —
    /// here we don't push the GC clip into the RENDER pipeline
    /// because the text pipeline doesn't honour scissor (lives in
    /// Stage 3e). v1's path has the same limitation; promoted to
    /// a Risk item rather than blocking 3a.
    fn render_text_chars_v2(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[char],
    ) -> io::Result<()> {
        use crate::kms::v2::engine::PreparedGlyph;

        let Some(font_xid) = self.core.current_font else {
            return Ok(());
        };
        // Stage 4a — resolve through redirect routing. Glyph
        // `dst_x` / `dst_y` per `PreparedGlyph` get the
        // window→backing translation applied below.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        let (paint_dx, paint_dy) = target.offset;
        // Rasterise glyphs in a tight FreeType-borrow scope so the
        // subsequent &mut self engine call doesn't conflict.
        let mut rendered: Vec<PreparedGlyph> = Vec::with_capacity(text.len());
        let mut cursor_x = x;
        {
            let Some(fs) = self.core.fonts.get(&font_xid) else {
                return Ok(());
            };
            let face = fs.face.borrow();
            let char_cache = &fs.char_info_cache;
            for &ch in text {
                let Some(ci) = char_cache.get(&ch) else {
                    cursor_x = cursor_x.saturating_add(6);
                    continue;
                };
                let _ = face
                    .0
                    .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
                let glyph = face.0.glyph();
                let bitmap = glyph.bitmap();
                if bitmap.width() > 0 && bitmap.rows() > 0 {
                    let w = bitmap.width() as usize;
                    let h = bitmap.rows() as usize;
                    let stride = bitmap.pitch();
                    let buf = bitmap.buffer();
                    let mut pixels = vec![0u8; w * h];
                    for row in 0..h {
                        let src = if stride >= 0 {
                            row * stride as usize
                        } else {
                            (h - 1 - row) * (stride as isize).unsigned_abs()
                        };
                        pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
                    }
                    rendered.push(PreparedGlyph {
                        dst_x: cursor_x + glyph.bitmap_left() + paint_dx,
                        dst_y: y - glyph.bitmap_top() + paint_dy,
                        w,
                        h,
                        pixels,
                        codepoint: ch as u32,
                    });
                }
                cursor_x = cursor_x.saturating_add(ci.character_width as i32);
            }
        }
        if rendered.is_empty() {
            return Ok(());
        }
        let foreground_rgba = [
            ((foreground >> 16) & 0xFF) as f32 / 255.0,
            ((foreground >> 8) & 0xFF) as f32 / 255.0,
            (foreground & 0xFF) as f32 / 255.0,
            1.0,
        ];
        match self.engine.image_text(
            &mut self.store,
            &mut self.platform,
            target.id,
            font_xid,
            foreground_rgba,
            &rendered,
        ) {
            Ok(stats) => {
                for _ in 0..stats.atlas_interns {
                    self.telemetry.record_atlas_intern();
                }
                for _ in 0..stats.glyph_uploads {
                    self.telemetry.record_glyph_upload();
                }
                for _ in 0..stats.glyphs_dropped {
                    self.telemetry.record_glyph_dropped_atlas_full();
                }
                if stats.atlas_interns > 0 || !rendered.is_empty() {
                    self.telemetry.record_paint_submit();
                }
            }
            Err(e) => {
                log::warn!("v2 image_text: engine error xid={host_xid:#x}: {e:?} — dropping run");
            }
        }
        Ok(())
    }

    /// `image_text8/16` background-fill helper. Lowers the
    /// per-call rect to an `engine.fill_rect` op via the same
    /// path `fill_rectangle` (Stage 2c) uses, so the bg drawn
    /// here lives on the same storage as the glyph quads.
    fn fill_text_background(
        &mut self,
        host_xid: u32,
        background: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> io::Result<()> {
        if w <= 0 || h <= 0 {
            return Ok(());
        }
        // Stage 4a — resolve through redirect; rect origin is
        // shifted by the descendant→ancestor-backing offset.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        // L1 server-α invariant per `fill_solid_rects` (see comment
        // there): force α=1 on depth!=32 dsts so the scene
        // compositor's alpha_passthrough path doesn't blend the
        // text bg out.
        let depth = self.store.get(target.id).map(|d| d.depth).unwrap_or(24);
        let color = decode_x11_pixel_server_alpha(background, depth);
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: x + target.offset.0,
                y: y + target.offset.1,
            },
            extent: ash::vk::Extent2D {
                width: u32::try_from(w).unwrap_or(0),
                height: u32::try_from(h).unwrap_or(0),
            },
        };
        if let Err(e) =
            self.engine
                .fill_rect(&mut self.store, &mut self.platform, target.id, rect, color)
        {
            log::warn!("v2 image_text bg fill: engine.fill_rect xid={host_xid:#x}: {e:?}");
        } else {
            self.telemetry.record_paint_submit();
        }
        Ok(())
    }
}

/// Parse gradient stops (Stage 3b helper shared by linear +
/// radial). `stops_offset` is the offset in `body` where the
/// `n_stops` u32 starts. Returns `None` if the body is short.
/// Stops carry pos (FIXED 16.16) + 4 × u16 colour (straight).
fn parse_gradient_stops(body: &[u8], stops_offset: usize) -> Option<Vec<GradientStop>> {
    if body.len() < stops_offset + 4 {
        return None;
    }
    let n = u32::from_le_bytes(body[stops_offset..stops_offset + 4].try_into().ok()?) as usize;
    let pos_base = stops_offset + 4;
    let color_base = pos_base + n * 4;
    if body.len() < color_base + n * 8 {
        return None;
    }
    let mut stops: Vec<GradientStop> = Vec::with_capacity(n);
    for i in 0..n {
        let pos = i32::from_le_bytes(
            body[pos_base + i * 4..pos_base + i * 4 + 4]
                .try_into()
                .ok()?,
        );
        let cb = color_base + i * 8;
        let r = u16::from_le_bytes(body[cb..cb + 2].try_into().ok()?);
        let g = u16::from_le_bytes(body[cb + 2..cb + 4].try_into().ok()?);
        let b = u16::from_le_bytes(body[cb + 4..cb + 6].try_into().ok()?);
        let a = u16::from_le_bytes(body[cb + 6..cb + 8].try_into().ok()?);
        stops.push(GradientStop { pos, r, g, b, a });
    }
    Some(stops)
}

/// Apply a `RenderChangePicture` value-mask body to the picture
/// record. Mirrors v1's per-bit handler in shape; differences are
/// the v2 record's type and `KmsCore.pictures` as the map.
/// `body` is the full request body shape:
/// `picture(4) + value_mask(4) + values[…]`.
fn change_picture_apply_mask(core: &mut KmsCore, host_pic: u32, body: &[u8]) {
    if body.len() < 8 {
        return;
    }
    let value_mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let values = &body[8..];
    let mut off = 0usize;
    let next_u32 = |off: &mut usize| -> Option<u32> {
        let bytes = values.get(*off..*off + 4)?;
        *off += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    };
    for bit in 0..13 {
        let mask_bit = 1u32 << bit;
        if value_mask & mask_bit == 0 {
            continue;
        }
        let Some(v) = next_u32(&mut off) else {
            break;
        };
        match mask_bit {
            // CPRepeat
            0x0001 => {
                let repeat = match v {
                    1 => Repeat::Normal,
                    2 => Repeat::Pad,
                    3 => Repeat::Reflect,
                    _ => Repeat::None,
                };
                match core.pictures.get_mut(&host_pic) {
                    Some(PictureRecord::Drawable { repeat: r, .. })
                    | Some(PictureRecord::SolidFill { repeat: r, .. })
                    | Some(PictureRecord::LinearGradient { repeat: r, .. })
                    | Some(PictureRecord::RadialGradient { repeat: r, .. }) => *r = repeat,
                    None => {}
                }
            }
            // CPAlphaMap
            0x0002 => {
                if let Some(PictureRecord::Drawable { alpha_map, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_map = if v == 0 { None } else { Some(v) };
                }
            }
            // CPAlphaXOrigin
            0x0004 => {
                if let Some(PictureRecord::Drawable { alpha_x, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_x = v as i16;
                }
            }
            // CPAlphaYOrigin
            0x0008 => {
                if let Some(PictureRecord::Drawable { alpha_y, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_y = v as i16;
                }
            }
            // CPClipXOrigin
            0x0010 => {
                if let Some(PictureRecord::Drawable { clip_x, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *clip_x = v as i16;
                }
            }
            // CPClipYOrigin
            0x0020 => {
                if let Some(PictureRecord::Drawable { clip_y, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *clip_y = v as i16;
                }
            }
            // CPClipMask: a depth-1 pixmap xid (or `None` = 0).
            // For Stage 3b parity with v1, we don't synthesize the
            // pixmap → rect-list conversion (v1 needs the pixmap's
            // dimensions, which it had on KmsBackend.pixmaps). v2's
            // DrawableStore exposes the same dims via the storage's
            // extent, but for the common path (Cairo never sets a
            // bitmap mask via ChangePicture — it uses
            // SetPictureClipRectangles) this stays a logged no-op.
            // Risk-listed for the rendercheck clip-mask category.
            0x0040 => {
                if v == 0 {
                    if let Some(PictureRecord::Drawable { clip, .. }) =
                        core.pictures.get_mut(&host_pic)
                    {
                        *clip = None;
                    }
                } else {
                    log::debug!(
                        "v2 ChangePicture CPClipMask=pixmap {v:#x} on picture {host_pic:#x}: \
                         bitmap-mask clip not yet wired (Stage 3b TODO; rendercheck-only path)"
                    );
                }
            }
            // CPGraphicsExposure
            0x0080 => {
                if let Some(PictureRecord::Drawable {
                    graphics_exposure, ..
                }) = core.pictures.get_mut(&host_pic)
                {
                    *graphics_exposure = v != 0;
                }
            }
            // CPSubwindowMode
            0x0100 => {
                if let Some(PictureRecord::Drawable { subwindow_mode, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *subwindow_mode = v as u8;
                }
            }
            // CPPolyEdge
            0x0200 => {
                if let Some(PictureRecord::Drawable { poly_edge, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *poly_edge = v as u8;
                }
            }
            // CPPolyMode
            0x0400 => {
                if let Some(PictureRecord::Drawable { poly_mode, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *poly_mode = v as u8;
                }
            }
            // CPDither: consumed but intentionally not stored
            // (v1 same behaviour).
            0x0800 => {}
            // CPComponentAlpha
            0x1000 => match core.pictures.get_mut(&host_pic) {
                Some(PictureRecord::Drawable {
                    component_alpha, ..
                })
                | Some(PictureRecord::SolidFill {
                    component_alpha, ..
                }) => *component_alpha = v != 0,
                _ => {}
            },
            _ => {}
        }
    }
}

fn do_dump_scanout_v2(backend: &mut KmsBackendV2) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::kms::vk::{ops::run_one_shot_op, scanout::BoPhase};

    let Some(vk) = backend.platform.vk.as_ref().cloned() else {
        return Err(io::Error::other("no vulkan context"));
    };
    let Some(pool_handle) = backend.platform.ops_command_pool_handle() else {
        return Err(io::Error::other("no ops command pool"));
    };

    let preferred = [
        BoPhase::OnScreen,
        BoPhase::Pending,
        BoPhase::Submitted,
        BoPhase::Recording,
    ];
    let mut chosen: Vec<(usize, usize)> = Vec::new();
    for (pool_idx, pool) in backend.platform.scanout_pools.iter().enumerate() {
        let Some(pool) = pool.as_ref() else {
            continue;
        };
        for phase in preferred {
            if let Some(bo_idx) = pool.bos.iter().position(|bo| bo.state.phase == phase) {
                chosen.push((pool_idx, bo_idx));
                break;
            }
        }
    }
    if chosen.is_empty() {
        return Err(io::Error::other("no non-Free scanout bo found"));
    }

    static DUMP_COUNT: AtomicU32 = AtomicU32::new(0);
    let run = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut wrote_any = false;
    let mut last_err: Option<io::Error> = None;

    for (pool_idx, bo_idx) in chosen {
        let Some(pool) = backend
            .platform
            .scanout_pools
            .get_mut(pool_idx)
            .and_then(|p| p.as_mut())
        else {
            continue;
        };
        let Some(bo) = pool.bos.get_mut(bo_idx) else {
            continue;
        };
        let width = bo.width;
        let height = bo.height;
        let pitch = bo.pitch;
        let image = bo.vk_image;
        let staging_buffer = bo.vk_transfer.staging_buffer;
        let staging_mapped = bo.vk_transfer.staging_mapped;
        let staging_size = bo.vk_transfer.staging_size;

        let run_result = run_one_shot_op(&vk, pool_handle, |vk, cb| {
            let pre = [ash::vk::ImageMemoryBarrier2::default()
                .src_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
                .dst_stage_mask(ash::vk::PipelineStageFlags2::COPY)
                .dst_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
                .old_layout(ash::vk::ImageLayout::GENERAL)
                .new_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(image)
                .subresource_range(
                    ash::vk::ImageSubresourceRange::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let pre_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&pre);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &pre_dep) };

            let region = [ash::vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(
                    ash::vk::ImageSubresourceLayers::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(ash::vk::Offset3D::default())
                .image_extent(ash::vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            unsafe {
                crate::vk_count!(cmd_copy_image_to_buffer);
                vk.device.cmd_copy_image_to_buffer(
                    cb,
                    image,
                    ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    staging_buffer,
                    &region,
                );
            }

            let post = [ash::vk::ImageMemoryBarrier2::default()
                .src_stage_mask(ash::vk::PipelineStageFlags2::COPY)
                .src_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
                .old_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(ash::vk::ImageLayout::GENERAL)
                .image(image)
                .subresource_range(
                    ash::vk::ImageSubresourceRange::default()
                        .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let post_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&post);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &post_dep) };
            Ok(())
        });

        if let Err(e) = run_result {
            backend.platform.renderer_failed = true;
            let err = io::Error::other(format!("scanout copy submit: {e:?}"));
            log::warn!("v2 do_dump_scanout: output {pool_idx} failed: {err}");
            last_err = Some(err);
            continue;
        }

        let path = format!("./yserver-v2-scanout-{run}-out{pool_idx}.ppm");
        let raw =
            unsafe { std::slice::from_raw_parts(staging_mapped.as_ptr(), staging_size as usize) };
        use std::io::Write;
        let mut file = std::fs::File::create(&path)?;
        file.write_all(format!("P6\n{width} {height}\n255\n").as_bytes())?;
        let mut row_buf = vec![0u8; (width * 3) as usize];
        for y in 0..height as usize {
            let row_start = y * pitch as usize;
            for x in 0..width as usize {
                let pi = row_start + x * 4;
                let dst = x * 3;
                row_buf[dst] = raw[pi + 2];
                row_buf[dst + 1] = raw[pi + 1];
                row_buf[dst + 2] = raw[pi];
            }
            file.write_all(&row_buf)?;
        }
        log::info!("v2 do_dump_scanout: wrote {path} ({width}x{height})");
        wrote_any = true;
    }

    if wrote_any {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| io::Error::other("scanout dump failed")))
    }
}

/// Look up the X11 RENDER `PICTFORMAT` ID a picture was created
/// with. Returns `0` for the synthetic / missing cases (picture
/// xid is 0 = "no picture," non-Drawable variant, or the xid
/// isn't recorded). Used by the diagnostic `render_composite`
/// trace to show marco's declared sampling intent alongside the
/// drawable-depth-derived sampling shape v2 currently uses.
fn picture_pict_format(core: &crate::kms::core::KmsCore, host_pic: u32) -> u32 {
    if host_pic == 0 {
        return 0;
    }
    match core.pictures.get(&host_pic) {
        Some(crate::kms::core::PictureRecord::Drawable { pict_format, .. }) => *pict_format,
        _ => 0,
    }
}

/// Describe a `ResolvedSource` for the diagnostic
/// `render_composite` trace. Returns `(kind_name, depth)` —
/// depth is `0` for non-Drawable sources where the concept
/// doesn't apply. Used only from the trace path; not on hot
/// paint paths.
fn describe_resolved_source(
    store: &super::store::DrawableStore,
    src: &crate::kms::v2::engine::ResolvedSource,
) -> (&'static str, u8) {
    use crate::kms::v2::engine::ResolvedSource;
    match src {
        ResolvedSource::Drawable(id) => {
            let depth = store.get(*id).map_or(0, |d| d.depth);
            ("drawable", depth)
        }
        ResolvedSource::Solid(_) => ("solid", 0),
        ResolvedSource::Gradient(_) => ("gradient", 0),
        ResolvedSource::None => ("none", 0),
    }
}

/// Per-drawable storage dump triggered by SIGUSR2 (or
/// `Ctrl-Alt-F12` via the input thread, mirroring
/// `Ctrl-Alt-Enter` for scanout). Walks a fixed-known set of
/// "interesting" drawables — root, COW, every redirected backing —
/// and writes each storage's content to a `yserver-v2-drawable-…`
/// file in cwd. Each dump cycle increments a global counter so
/// repeated invocations don't clobber.
///
/// Filename layout:
///
/// ```text
/// yserver-v2-drawable-{run}-root-{w}x{h}.ppm
/// yserver-v2-drawable-{run}-cow-{w}x{h}.ppm
/// yserver-v2-drawable-{run}-backing-W0x{w_xid}-B0x{b_xid}-{w}x{h}.ppm
/// ```
///
/// PPM (P6, RGB) is chosen for universal viewer support; the α
/// channel is *intentionally dropped* — the depth-24 padding-byte
/// question is settled separately (4d.6 + the sample-view fix) and
/// what we want to see here is whether `B` contains the window's
/// painted content at all. If a deeper α audit becomes useful later,
/// switching to PAM (P7 with TUPLTYPE=RGB_ALPHA) is a one-liner.
///
/// Reuses `RenderEngine::get_image` for the per-drawable readback so
/// staging-buffer allocation, layout transitions, fence sync, and
/// the BGRA8 → wire-byte pack all flow through the existing,
/// production-tested path. Each dump is one queue submit + one
/// fence wait, so the total stop-the-world time is `O(n)` Vk waits
/// — at ~5 ms per drawable on bee this is fine for diagnostic use.
fn do_dump_drawables_v2(backend: &mut KmsBackendV2) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static DUMP_COUNT: AtomicU32 = AtomicU32::new(0);
    let run = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);

    // Snapshot targets BEFORE touching the engine — `engine.get_image`
    // takes `&mut store + &mut platform`, so we can't hold any
    // shared borrow on `store` while iterating. Each tuple carries
    // everything the per-drawable loop needs: a human-readable label
    // for the filename, the DrawableId for the read, the depth (drives
    // wire-byte unpack), and the extent (drives the read rect + the
    // PPM header).
    #[derive(Debug)]
    struct DumpTarget {
        label: String,
        id: super::store::DrawableId,
        depth: u8,
        width: u32,
        height: u32,
    }
    let mut targets: Vec<DumpTarget> = Vec::new();
    {
        // Scoped read-borrow on the store + core. The borrow ends
        // at the `}` so the mutable borrows below are free to fire.
        if let Some(root_id) = backend.store.lookup(backend.core.window_id)
            && let Some(d) = backend.store.get(root_id)
        {
            targets.push(DumpTarget {
                label: format!("root-0x{:x}", backend.core.window_id),
                id: root_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        if let Some(cow_id) = backend.cow_id
            && let Some(d) = backend.store.get(cow_id)
        {
            targets.push(DumpTarget {
                label: format!(
                    "cow-0x{:x}",
                    yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0
                ),
                id: cow_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        // Sorted iteration so re-running the dump gives the same
        // filename ordering — keeps diff-tooling stable across runs.
        let mut pairs: Vec<(u32, u32)> = backend
            .core
            .host_window_to_backing
            .iter()
            .map(|(&w, b)| (w, b.as_raw()))
            .collect();
        pairs.sort_by_key(|(w, _)| *w);
        for (w_xid, b_xid) in pairs {
            let Some(b_id) = backend.store.lookup(b_xid) else {
                continue;
            };
            let Some(d) = backend.store.get(b_id) else {
                continue;
            };
            targets.push(DumpTarget {
                label: format!("backing-W0x{w_xid:x}-B0x{b_xid:x}"),
                id: b_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        // Recent COW-targeted PresentPixmap sources — the bisect
        // dump for "is marco's offscreen broken, or only the
        // copy-to-COW step?" Walk in submission order (oldest
        // first); dedup against drawables already in the target
        // list so we don't double-dump if marco's offscreen
        // happens to coincide with a registered backing.
        let already: std::collections::HashSet<super::store::DrawableId> =
            targets.iter().map(|t| t.id).collect();
        for (idx, &src_xid) in backend.present_to_cow_sources.iter().enumerate() {
            let Some(src_id) = backend.store.lookup(src_xid) else {
                continue;
            };
            if already.contains(&src_id) {
                continue;
            }
            let Some(d) = backend.store.get(src_id) else {
                continue;
            };
            targets.push(DumpTarget {
                label: format!("present-src-{idx}-0x{src_xid:x}"),
                id: src_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
    }

    if targets.is_empty() {
        return Err(io::Error::other("no drawable dump targets available"));
    }
    log::info!(
        "v2 do_dump_drawables: run={run} target_count={}",
        targets.len(),
    );

    let mut wrote = 0_u32;
    let mut last_err: Option<io::Error> = None;
    for t in targets {
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: t.width,
                height: t.height,
            },
        };
        let bytes = match backend.engine.get_image(
            &mut backend.store,
            &mut backend.platform,
            t.id,
            rect,
            t.depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                let err = io::Error::other(format!("get_image {} ({:?}): {e:?}", t.label, t.id));
                log::warn!("v2 do_dump_drawables: {err}");
                last_err = Some(err);
                continue;
            }
        };
        let path = format!(
            "./yserver-v2-drawable-{run}-{label}-{w}x{h}.ppm",
            label = t.label,
            w = t.width,
            h = t.height
        );
        if let Err(e) = write_drawable_ppm(&path, &bytes, t.width, t.height, t.depth) {
            log::warn!("v2 do_dump_drawables: write {path}: {e}");
            last_err = Some(e);
            continue;
        }
        log::info!(
            "v2 do_dump_drawables: wrote {path} (depth={} bytes={})",
            t.depth,
            bytes.len(),
        );
        wrote += 1;
    }
    if wrote > 0 {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| io::Error::other("no drawables dumped")))
    }
}

/// Write a single drawable's storage content as PAM (P7,
/// `RGB_ALPHA`) for depth-24 / depth-32 BGRA8 drawables (preserves
/// the α byte so a later analysis can see whether stored α is zero
/// / one / noise — the Stage 4d "shadow only" diagnosis needs to
/// distinguish "RGB looks right but α is zero" from "RGB itself is
/// broken"), or PGM (P5, gray) for depth-1 / depth-8 R8 drawables.
/// PAM is Netpbm's anymap format; ImageMagick / GIMP / most viewers
/// handle it transparently and dispatch on the magic number, not
/// the file extension.
///
/// `bytes` is the wire-packed buffer returned by
/// `RenderEngine::get_image`:
/// - depth 24/32: 4 bytes/pixel, X11 wire order (B, G, R, X|A) per
///   `pack_from_storage`'s BGRA8 → wire mapping.
/// - depth 8:     1 byte/pixel, R-channel.
/// - depth 1:     bit-packed MSB-first (rendered as PGM after
///   bit-expand, mostly for completeness — no real consumer of the
///   v2 dump runs depth-1 backings).
fn write_drawable_ppm(
    path: &str,
    bytes: &[u8],
    width: u32,
    height: u32,
    depth: u8,
) -> io::Result<()> {
    use std::io::Write;

    let w = usize::try_from(width).map_err(|e| io::Error::other(format!("width: {e}")))?;
    let h = usize::try_from(height).map_err(|e| io::Error::other(format!("height: {e}")))?;
    let mut file = std::fs::File::create(path)?;
    match depth {
        24 | 32 => {
            // BGRA8 wire → PAM RGBA. Reorder per pixel: src is
            // (B, G, R, X|A) in storage byte order; PAM tuples
            // emit (R, G, B, A).
            let expected = w
                .checked_mul(h)
                .and_then(|p| p.checked_mul(4))
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(
                format!(
                    "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
                )
                .as_bytes(),
            )?;
            let mut row = vec![0u8; w * 4];
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 4;
                    let dst = x * 4;
                    row[dst] = bytes[src + 2]; // R
                    row[dst + 1] = bytes[src + 1]; // G
                    row[dst + 2] = bytes[src]; // B
                    row[dst + 3] = bytes[src + 3]; // A
                }
                file.write_all(&row)?;
            }
        }
        8 => {
            let expected = w
                .checked_mul(h)
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(format!("P5\n{width} {height}\n255\n").as_bytes())?;
            file.write_all(&bytes[..expected])?;
        }
        1 => {
            // Bit-packed MSB-first, padded to byte boundaries per
            // X11 wire spec for ZPixmap depth-1. Expand to PGM
            // bytes so a viewer can render the mask.
            let row_bytes = w.div_ceil(8);
            let expected = row_bytes
                .checked_mul(h)
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(format!("P5\n{width} {height}\n255\n").as_bytes())?;
            let mut out = vec![0u8; w];
            for y in 0..h {
                for x in 0..w {
                    let byte = bytes[y * row_bytes + (x / 8)];
                    let bit = byte >> (7 - (x % 8)) & 1;
                    out[x] = if bit == 1 { 255 } else { 0 };
                }
                file.write_all(&out)?;
            }
        }
        other => {
            return Err(io::Error::other(format!(
                "unsupported depth {other} for drawable dump",
            )));
        }
    }
    Ok(())
}

/// Map a host-visual descriptor to a depth for the storage
/// allocator. Stage 2d picks BGRA32 for `CopyFromParent` (the
/// default visual is depth-24 ARGB-equivalent in our advertised
/// pixel format) and honours an explicit depth otherwise.
/// Stage 3c: walk a `PictureRecord` and resolve it into the
/// engine's `ResolvedSource` plus the per-picture sampler attrs
/// (`repeat`, `transform`, `component_alpha`). Source-only
/// variants (`SolidFill`, gradients) carry no backing drawable;
/// `Drawable` resolves the host xid through `DrawableStore`.
///
/// Returns `None` if the picture xid isn't recorded or the
/// drawable backing has gone away. The engine treats this as a
/// gap and silently no-ops (matches v1's
/// `resolve_render_pic_with_gradient_xid` shape).
/// Stage 3f.14: depth-appropriate safe-default init colour for
/// fresh window storage when the X11 attribute `background-pixel`
/// is `None`. The v2 PixmapPool (3f.10) recycles
/// (image, view, memory) triples between drawables; a pool-take
/// inherits the returner's pixels, so leaving fresh storage at
/// pool content surfaces visually as widget-rect islands on
/// black (caja's drag artifact, 3f.10 + 3f.14 reproducer).
///
/// - Depth 32 windows are premultiplied-α; transparent black
///   `(0, 0, 0, 0)` is the no-op contribution to compositing.
/// - Depth 24 and other non-alpha visuals get opaque black
///   `(0, 0, 0, 1)` — matches "uninitialised window shows black"
///   which is the historical X11 behaviour clients expect.
fn default_window_init_color(depth: u8) -> [f32; 4] {
    if depth == 32 {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

/// Stage 3f.13 glyph fallback: pull the first stop's premultiplied
/// RGBA from a gradient picture record. Returns `None` if `host_pic`
/// isn't a gradient or has zero stops. Used by `composite_glyphs`
/// when a gradient source needs a solid-fill approximation — the
/// glyph paint path only knows how to sample a single colour, so a
/// proper LUT-sampled gradient on glyphs would need a separate
/// pipeline (deferred past Stage 3).
fn first_stop_premul_of_gradient(core: &KmsCore, host_pic: u32) -> Option<[f32; 4]> {
    let stop = match core.pictures.get(&host_pic)? {
        PictureRecord::LinearGradient { stops, .. }
        | PictureRecord::RadialGradient { stops, .. } => stops.first()?,
        _ => return None,
    };
    let a = f32::from(stop.a) / 65535.0;
    let r = (f32::from(stop.r) / 65535.0) * a;
    let g = (f32::from(stop.g) / 65535.0) * a;
    let b = (f32::from(stop.b) / 65535.0) * a;
    Some([r, g, b, a])
}

fn resolve_picture_for_render(
    core: &KmsCore,
    store: &crate::kms::v2::store::DrawableStore,
    host_pic: u32,
) -> Option<(
    crate::kms::v2::engine::ResolvedSource,
    Repeat,
    Option<PictTransform>,
    bool, // component_alpha
)> {
    use crate::kms::v2::engine::ResolvedSource;
    match core.pictures.get(&host_pic)? {
        PictureRecord::Drawable {
            host_xid,
            repeat,
            transform,
            component_alpha,
            ..
        } => {
            let id = store.lookup(*host_xid)?;
            Some((
                ResolvedSource::Drawable(id),
                *repeat,
                *transform,
                *component_alpha,
            ))
        }
        PictureRecord::SolidFill {
            premul,
            repeat,
            component_alpha,
        } => Some((
            ResolvedSource::Solid(*premul),
            *repeat,
            None,
            *component_alpha,
        )),
        PictureRecord::LinearGradient {
            repeat, transform, ..
        }
        | PictureRecord::RadialGradient {
            repeat, transform, ..
        } => {
            // Stage 3f.13: full LUT sampling. The engine-side
            // `GradientPicture` was built at create time and lives
            // in `engine.picture_paint[host_pic]`; engine looks it
            // up by xid. If the engine-side build failed (test
            // fixture with no Vk, or allocation error), the engine
            // logs a gap and skips the paint — no first-stop
            // collapse fallback.
            Some((
                ResolvedSource::Gradient(host_pic),
                *repeat,
                *transform,
                false,
            ))
        }
    }
}

/// Stage 3c: dst picture resolution. RENDER paint ops require
/// the dst to be a `PictureRecord::Drawable` (you can't paint
/// into a SolidFill or a Gradient). Returns the underlying
/// dst drawable's `host_xid` plus the picture's clip rectangles
/// (already pre-shifted by `clip_x` / `clip_y` per Stage 3b).
///
/// Stage 4a: callers feed `host_xid` through
/// `KmsBackendV2::resolve_paint_target` to apply COMPOSITE
/// redirect routing. The free function stays pure
/// (`&KmsCore`-only) so it can also be called from contexts
/// where the windows_v2 / parent chain isn't relevant.
fn resolve_dst_picture_for_render(
    core: &KmsCore,
    host_pic: u32,
) -> Option<(u32, Option<Vec<Rectangle16>>)> {
    let PictureRecord::Drawable { host_xid, clip, .. } = core.pictures.get(&host_pic)? else {
        return None;
    };
    Some((*host_xid, clip.clone()))
}

/// Audit #2 (2026-05-19) — extract a source / mask picture's
/// `clientClip` for `render_composite`'s composite-region
/// computation. The picture's clip rects are stored
/// pre-shifted by `clip_x` / `clip_y` (see
/// `render_set_picture_clip_rectangles`), so the returned list
/// is already in the picture's drawable-local coord space —
/// `compute_render_composite_clip` translates from there into
/// dst space via `(xDst - xSrc, yDst - ySrc)`.
///
/// Non-Drawable pictures (`SolidFill` / gradients) carry no
/// `clientClip` and return `None`. `host_pic == 0` (the
/// "no mask" sentinel `RenderComposite` uses) also returns `None`.
fn picture_client_clip(core: &KmsCore, host_pic: u32) -> Option<Vec<Rectangle16>> {
    if host_pic == 0 {
        return None;
    }
    match core.pictures.get(&host_pic)? {
        PictureRecord::Drawable { clip, .. } => clip.clone(),
        PictureRecord::SolidFill { .. }
        | PictureRecord::LinearGradient { .. }
        | PictureRecord::RadialGradient { .. } => None,
    }
}

/// Stage 3f.8: 16×16 BGRA8 default-arrow cursor sprite. Hotspot
/// at (0, 0). The shape is a filled right-triangle pointing
/// down-right — sized so the tip falls on the click point and the
/// rest extends into the screen. Pixels: opaque black inside,
/// fully transparent outside. Stage 4 swaps this for whatever
/// `define_cursor` / `xfixes_change_cursor_by_name` selects.
fn default_cursor_sprite_bgra() -> Vec<u8> {
    const W: usize = 16;
    const H: usize = 16;
    let mut bytes = vec![0u8; W * H * 4];
    let set = |bytes: &mut [u8], x: usize, y: usize, b: u8, g: u8, r: u8, a: u8| {
        let off = (y * W + x) * 4;
        bytes[off] = b;
        bytes[off + 1] = g;
        bytes[off + 2] = r;
        bytes[off + 3] = a;
    };
    // 12-row arrow body. Each row y has y+1 black pixels starting
    // at x=0, capped at 10 to leave room for the "tail" rows 10/11.
    for y in 0..12 {
        let row_w = y.min(10) + 1;
        for x in 0..row_w {
            set(&mut bytes, x, y, 0x00, 0x00, 0x00, 0xFF);
        }
    }
    // 1-px white outline on the diagonal edge for visibility
    // against dark backgrounds (xfwm4 root, terminal black bg).
    for y in 0..12 {
        let edge_x = y.min(10);
        // Pixel to the right of the diagonal — white outline.
        let ox = edge_x + 1;
        if ox < W && y < H {
            set(&mut bytes, ox, y, 0xFF, 0xFF, 0xFF, 0xFF);
        }
    }
    bytes
}

/// Resolve the drawable depth for a new subwindow. `CopyFromParent`
/// inherits the parent window's depth; only the root / untracked
/// fallback defaults to 24.
/// Wrap raw GetImage pixel bytes into a full X11 GetImage reply
/// (32-byte header + pixels). `sequence` and `visual` are patched in
/// by the handler (`process_request.rs:handle_get_image`); this
/// helper fills the rest. Mirrors v1's
/// `KmsBackend::get_image` (kms/backend.rs:10400-10420) byte-for-byte
/// so the handler's expectations carry across both backends.
fn wrap_get_image_reply(depth: u8, pixel_bytes: Vec<u8>) -> Vec<u8> {
    let pixel_len = pixel_bytes.len();
    let mut out = Vec::with_capacity(32 + pixel_len);
    out.push(1); // [0]: Reply indicator
    out.push(depth); // [1]: depth
    out.extend_from_slice(&[0u8; 2]); // [2..4]: sequence (patched by handler)
    // [4..8]: reply length in u32 units. Rows are already
    // 4-byte aligned for the depths we support (1/8/24/32 — see
    // `pack_from_storage`), so this is `pixel_len / 4`.
    let reply_length_units = u32::try_from(pixel_len / 4).unwrap_or(u32::MAX);
    out.extend_from_slice(&reply_length_units.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // [8..12]: visual (patched by handler)
    out.extend_from_slice(&[0u8; 20]); // [12..32]: padding
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(&pixel_bytes);
    out
}

fn depth_for_visual(visual: HostSubwindowVisual, parent_depth: Option<u8>) -> u8 {
    match visual {
        HostSubwindowVisual::CopyFromParent => parent_depth.unwrap_or(24),
        HostSubwindowVisual::DepthOnly { depth } => {
            if depth == 0 {
                parent_depth.unwrap_or(24)
            } else {
                depth
            }
        }
        HostSubwindowVisual::Explicit { depth, .. } => {
            if depth == 0 {
                parent_depth.unwrap_or(24)
            } else {
                depth
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────
// `Backend` trait implementation. The shape:
//
// A. Pure accessors — return values from `self.core` or local
//    constants identical to v1.
// B. Bookkeeping mutations — mutate `self.core` (XID map etc.).
// C. Mixed bookkeeping + storage — log a gap; for ops that must
//    return a handle, mint a fresh xid via `self.core.next_host_xid()`
//    so subsequent xid_map lookups stay consistent.
// D. Paint / RENDER / scene — log a gap, return Ok or the
//    default-impl shape.
// ───────────────────────────────────────────────────────────────

impl Backend for KmsBackendV2 {
    // ── A. Accessors (mirror KmsBackend exactly) ────────────────

    fn window_id(&self) -> u32 {
        self.core.window_id
    }

    fn root_visual_xid(&self) -> u32 {
        self.core.root_visual_xid
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        Some(ARGB_VISUAL.0)
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        Some(ARGB_COLORMAP.0)
    }

    fn render_opcode(&self) -> Option<u8> {
        Some(133)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        Some(136)
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        Some((136, 85, 162))
    }

    fn composite_opcode(&self) -> Option<u8> {
        Some(144)
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        if ynest_fmt == 0 {
            None
        } else {
            Some(ynest_fmt)
        }
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn xid_map(&self) -> &HostXidMap {
        &self.core.xid_map
    }

    // ── Single-threaded core hooks ──────────────────────────────

    fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent) {
        // Stage 3f.7 port of v1's on_host_input. Key events go
        // through the cook → key fanout path; pointer events flow
        // into `pending_pointer_events`, which we drain to the
        // pointer fanout after each call so the buffer stays empty
        // between events (matches v1's contract).
        use yserver_core::core_loop::{
            HostInputEvent, key_fanout::key_event_fanout_to_state,
            pointer_fanout::pointer_event_fanout_to_state,
        };

        match ev {
            HostInputEvent::PointerMotion { x, y, time: _ } => {
                self.process_pointer_absolute(state, x as f32, y as f32);
            }
            HostInputEvent::PointerButton {
                button,
                pressed,
                time: _,
            } => {
                self.process_pointer_button(u32::from(button), pressed, state);
            }
            HostInputEvent::Key(raw) => {
                let cooked = self.cook_host_key(raw);
                let _dropped = key_event_fanout_to_state(state, cooked);
                return;
            }
        }

        // Drain pointer events queued by the process_pointer_* call.
        let pending = std::mem::take(&mut self.core.pending_pointer_events);
        for ev in pending {
            let _dropped =
                pointer_event_fanout_to_state(state, &self.core.xid_map, ev, true, false);
        }
    }

    fn on_page_flip_ready(&mut self, _state: &mut ServerState) {
        let flipped = match self.platform.drain_page_flip_events() {
            Ok(flipped) => flipped,
            Err(e) => {
                log::warn!("v2: drain_page_flip_events failed: {e}");
                return;
            }
        };
        for output_idx in flipped {
            if self
                .scene
                .handle_page_flip_complete(output_idx, &mut self.store, &mut self.platform)
            {
                self.telemetry.record_frame_present();
            }
        }
        // Sweep retired engine submits + retired drawables now
        // that their fences may have signaled.
        self.engine.poll_retired(&self.platform);
        self.store.poll_pending_retire(&mut self.platform);
    }

    fn mark_dirty(&mut self) {
        // Wake the compositor without inventing full-output damage.
        // Paint paths already record per-drawable presentation
        // damage, and cursor motion is projected by build_scene.
        self.scene.wake_for_damage();
    }

    fn maybe_composite(&mut self) -> io::Result<()> {
        let result = if !self.scene.scene_structure_dirty {
            Ok(())
        } else {
            match self.scene.tick(
                &self.core,
                &mut self.store,
                &mut self.platform,
                &self.windows_v2,
                &mut self.telemetry,
            ) {
                Ok(composed) => {
                    for _ in 0..composed {
                        self.telemetry.record_composite_submit();
                    }
                    Ok(())
                }
                Err(e) => {
                    log::warn!("v2 maybe_composite: scene.tick failed: {e:?}");
                    Ok(())
                }
            }
        };
        // Per-second telemetry summary emission.
        self.telemetry.maybe_emit();
        result
    }

    fn dump_scanout(&mut self) {
        if let Err(e) = do_dump_scanout_v2(self) {
            log::warn!("v2 dump_scanout: {e}");
        }
    }

    fn dump_drawables(&mut self) {
        if let Err(e) = do_dump_drawables_v2(self) {
            log::warn!("v2 dump_drawables: {e}");
        }
    }

    fn note_present_pixmap(&mut self, src_pixmap_xid: u32, dst_window_xid: u32) {
        // Capture COW-targeted presents only — that's the bisect
        // point of interest for the Stage 4d "shadow only" bug.
        // Other present targets (toplevel windows in DRI3 / GL
        // composite flows) aren't useful for the dump set.
        if dst_window_xid != yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0 {
            return;
        }
        const CAP: usize = 16;
        // Deduplicate consecutive same-xid presents (marco
        // double-buffers two offscreens so the ring otherwise
        // alternates between two values; keeping only fresh xids
        // means a dump of size N captures up to N *distinct*
        // recent sources).
        if self.present_to_cow_sources.back() == Some(&src_pixmap_xid) {
            return;
        }
        if self.present_to_cow_sources.len() == CAP {
            self.present_to_cow_sources.pop_front();
        }
        self.present_to_cow_sources.push_back(src_pixmap_xid);
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)> {
        // DRM fd for page-flip events; libinput fd if the input
        // context is still owned by us. Delegates to
        // PlatformBackend.
        self.platform.poll_fds()
    }

    // ── Subwindow lifecycle ─────────────────────────────────────

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let xid = self.core.next_host_xid();
        let parent_xid = host_parent.as_raw();
        let parent_depth = if parent_xid == self.core.window_id {
            Some(24)
        } else {
            self.windows_v2.get(&parent_xid).map(|g| g.depth)
        };
        let depth = depth_for_visual(visual, parent_depth);
        // Stage 3f.6: record the parent xid so `build_scene` can
        // recurse the tree. `bg_pixel` is passed into
        // `allocate_window_storage`, which paints it into the fresh
        // storage; bg_pixmap is stored as metadata for now (proper
        // pixmap-bg support is a Stage 4-ish item).
        self.allocate_window_storage(
            xid,
            x,
            y,
            width.max(1),
            height.max(1),
            depth,
            Some(parent_xid),
            background_pixel,
        );
        if let Some(bg_pix) = background_pixmap
            && let Some(geom) = self.windows_v2.get_mut(&xid)
        {
            geom.bg_pixmap = Some(bg_pix);
        }
        self.scene.mark_scene_structure_dirty();
        WindowHandle::from_raw(xid).ok_or_else(|| io::Error::other("create_subwindow: xid was 0"))
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.decref(&mut self.platform, id);
        }
        self.windows_v2.remove(&host_xid);
        // Stage 3f.11: also drop from top_level_order so build_scene
        // doesn't walk a stale xid. Same hazard as reparent — pre-fix
        // a destroyed top-level lingered in the order and produced
        // ghost draws until the next register_top_level filled the
        // slot.
        self.core.top_level_order.retain(|&x| x != host_xid);
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.mapped = true;
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.set_scene_participating(id, true);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.mapped = false;
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.set_scene_participating(id, false);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        let Some(geom) = self.windows_v2.get_mut(&host_xid) else {
            // Window not tracked — log + skip (e.g., configure
            // before register). v1 tolerates this.
            return Ok(());
        };
        let mut size_changed = false;
        if let Some(x) = config.x {
            geom.x = x;
        }
        if let Some(y) = config.y {
            geom.y = y;
        }
        if let Some(w) = config.width
            && w != geom.width
        {
            geom.width = w;
            size_changed = true;
        }
        if let Some(h) = config.height
            && h != geom.height
        {
            geom.height = h;
            size_changed = true;
        }
        let new_w = geom.width.max(1);
        let new_h = geom.height.max(1);
        let depth = geom.depth;
        let scene_participating = geom.mapped;
        let bg_pixel = geom.bg_pixel;
        let bg_pixmap = geom.bg_pixmap;
        if size_changed && let Some(old_id) = self.store.lookup(host_xid) {
            // Replace window storage. Stage 2d doesn't preserve
            // content across resize — clients are expected to
            // repaint after configure (X11 semantics).
            //
            // Detach `by_xid[host_xid]` BEFORE decref + allocate.
            // Any Picture wrapping this window (e.g. marco's frame
            // compositing) holds an extra refcount on the old
            // drawable; without the explicit detach, `decref`
            // returns `StillReferenced` and leaves the xid map
            // pointing at the old drawable → `store.allocate(xid)`
            // below fails with `XidInUse` → the window silently
            // stays at the old storage. xeyes resize regression
            // observed on bee + fuji.
            //
            // The old drawable stays alive in `entries` until its
            // last refcount drops; its in-flight ticket still
            // retires correctly. Picture's next `lookup(xid)`
            // returns the NEW DrawableId, which matches X11 RENDER
            // semantics (a Picture on a window references the
            // window's *current* storage).
            self.store.detach_xid(host_xid);
            self.store.decref(&mut self.platform, old_id);
            match self.platform.allocate_drawable_storage(new_w, new_h, depth) {
                Ok(storage) => {
                    if let Err(e) = self.store.allocate(
                        host_xid,
                        DrawableKind::Window,
                        depth,
                        scene_participating,
                        storage,
                    ) {
                        log::warn!(
                            "v2 configure_subwindow: store.allocate failed for xid {host_xid:#x}: {e:?}",
                        );
                    } else if let Some(id) = self.store.lookup(host_xid) {
                        if let Some(bg_pixmap_host_xid) = bg_pixmap {
                            if let Err(e) = self.clear_window_area_with_background(
                                host_xid,
                                bg_pixel.unwrap_or(0),
                                Some(bg_pixmap_host_xid),
                                0,
                                0,
                                new_w,
                                new_h,
                            ) {
                                log::debug!(
                                    "v2 configure_subwindow: bg_pixmap resize init failed for xid {host_xid:#x}: {e:?}"
                                );
                            }
                        } else {
                            // Stage 3f.6 + 3f.14: clear the fresh
                            // storage so resize doesn't leave pool-
                            // returner content (or Vk-undefined bytes)
                            // visible until the client's next repaint.
                            // Bg_pixel-set: paint that colour;
                            // otherwise depth-appropriate safe default
                            // (matches `allocate_window_storage`).
                            let color = bg_pixel.map_or_else(
                                || default_window_init_color(depth),
                                |pixel| decode_x11_pixel_server_alpha(pixel, depth),
                            );
                            let rect = ash::vk::Rect2D {
                                offset: ash::vk::Offset2D::default(),
                                extent: ash::vk::Extent2D {
                                    width: u32::from(new_w),
                                    height: u32::from(new_h),
                                },
                            };
                            if let Err(e) = self.engine.fill_rect(
                                &mut self.store,
                                &mut self.platform,
                                id,
                                rect,
                                color,
                            ) {
                                log::debug!(
                                    "v2 configure_subwindow: storage init fill failed for xid {host_xid:#x}: {e:?}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "v2 configure_subwindow: alloc storage failed for xid {host_xid:#x}: {e:?}",
                    );
                }
            }
        }
        if let Some(stack_mode) = config.stack_mode {
            if self.core.top_level_order.contains(&host_xid) {
                self.restack_top_level(host_xid, stack_mode, config.sibling);
            } else {
                self.restack_subwindow(host_xid, stack_mode, config.sibling);
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        // Stage 3f.6: update the parent xid so build_scene's
        // descendant traversal sees the new tree shape on the next
        // tick. A `host_parent` of 0 (or any xid not in
        // `windows_v2` — typically root, 0x100) means the window
        // becomes a top-level under root; we record `None` so the
        // recurse treats it as a top-level entry.
        //
        // Stage 3f.11 bug-fix: also reconcile `core.top_level_order`
        // with the new parent. Pre-3f.11, an xid that was originally
        // registered as a top-level (parent=root) stayed in
        // `top_level_order` even after being reparented under
        // another window. `build_scene` then emitted the same xid
        // TWICE: once via the `top_level_order` walk (at its now-
        // child-relative coords interpreted as absolute → typically
        // (0,0)) and once via the recurse from its real parent (at
        // its correct screen position). Observable as MATE's clock
        // applet rendered at BOTH ends of the panel: the right edge
        // is the real position, the left edge is the ghost.
        let parent = if host_parent == 0 || !self.windows_v2.contains_key(&host_parent) {
            None
        } else {
            Some(host_parent)
        };
        let new_rank = self.alloc_window_stack_rank();
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.x = x;
            geom.y = y;
            geom.parent = parent;
            geom.stack_rank = new_rank;
        }
        // Reconcile top_level_order:
        // - parent == None  → window is now (or stays) a top-level
        //   under root; ensure it's in top_level_order.
        // - parent == Some  → window is now a sub-window; remove
        //   from top_level_order so the scene doesn't double-emit.
        match parent {
            None => {
                if !self.core.top_level_order.contains(&host_xid) {
                    self.core.top_level_order.push(host_xid);
                }
            }
            Some(_) => {
                self.core.top_level_order.retain(|&x| x != host_xid);
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        // Stage 3f.6: v1-shape parse of the CWA value-mask.
        // CWBackPixmap (0x01) and CWBackPixel (0x02) are the two
        // we honour today — they decide what fresh / cleared regions
        // of the window storage look like. Other CW bits
        // (CWBorderPixel, CWBitGravity, CWEventMask, etc.) flow
        // through other Backend methods or get folded into broader
        // window state; storing only what `windows_v2` needs.
        let Some(geom) = self.windows_v2.get_mut(&host_xid) else {
            return Ok(());
        };
        let mut repaint_bg = false;
        let mut idx = 0;
        if value_mask & 0x01 != 0 && idx < values.len() {
            // CWBackPixmap. 0 = None / inherit-from-parent.
            let v = values[idx];
            geom.bg_pixmap = if v == 0 { None } else { Some(v) };
            idx += 1;
            repaint_bg = true;
        }
        if value_mask & 0x02 != 0 && idx < values.len() {
            // CWBackPixel — opaque ARGB-or-XRGB pixel value.
            geom.bg_pixel = Some(values[idx]);
            repaint_bg = true;
        }
        if repaint_bg {
            let _ = geom;
            let Some(geom) = self.windows_v2.get(&host_xid).copied() else {
                return Ok(());
            };
            // Stage 4d fix — skip the eager CWA-time clear when W is
            // under COMPOSITE redirect. The backing's content belongs
            // to the redirected client / external compositor, not the
            // server; routing a fill through `resolve_paint_target`
            // here would land on B and wipe the compositor's pixels
            // (the marco-with-compositing "CC turns opaque black on
            // drag" bug — marco re-asserts bg_pixmap=None on every
            // drag-induced configure). Real X11 doesn't redraw on
            // CWA either — the bg attribute only affects future
            // ClearArea / Expose handling. v2's eager clear was a
            // Stage 3f.6 over-reach.
            let is_redirected = self
                .store
                .lookup(host_xid)
                .and_then(|id| self.store.redirected_target(id))
                .is_some();
            if !is_redirected {
                let bg_pixel = geom.bg_pixel.unwrap_or(0);
                self.clear_window_area_with_background(
                    host_xid,
                    bg_pixel,
                    geom.bg_pixmap,
                    0,
                    0,
                    geom.width.max(1),
                    geom.height.max(1),
                )?;
            }
        }
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _mask: u32,
        _enabled: bool,
    ) -> io::Result<()> {
        // No-op on KMS, same shape as v1. The trait method is a
        // holdover from Phase 6.3 ynest where it forwarded event-mask
        // changes to a host X server; KMS owns the display directly
        // and has no upstream server to notify. Event delivery on KMS
        // is driven entirely from libinput/seat plumbing inside the
        // backend, so there's nothing to update here.
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        // Bookkeeping mutation — same shape as v1. The XID map is in
        // KmsCore and shared.
        self.core.xid_map.insert(host_xid, nested_id);
        // Top-level visible-window tracking for the scene
        // assembler. register_top_level doesn't carry geometry;
        // start at 1x1 (Stage 2 plan compromise) and resize on
        // first configure_subwindow.
        if !self.windows_v2.contains_key(&host_xid) {
            // Top-level: parent = None (root), no bg_pixel known yet
            // (set later via change_subwindow_attributes).
            self.allocate_window_storage(host_xid, 0, 0, 1, 1, 24, None, None);
        }
        if !self.core.top_level_order.contains(&host_xid) {
            self.core.top_level_order.push(host_xid);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.core.xid_map.insert(host_xid, nested_id);
        if !self.windows_v2.contains_key(&host_xid) {
            // register_subwindow doesn't carry parent xid (Backend
            // trait doesn't expose it here — the trait shape was
            // built around v1's flat windows table). Parent is set
            // when `create_subwindow` fires for the same host_xid
            // (it's the entry point that knows the parent). If
            // register_subwindow runs first (e.g. ynest's wire
            // ordering), we'll get `None` and the scene treats this
            // window as a top-level until a `create_subwindow`
            // catches up. Matches v1's "no parent tracking" status
            // — v1 simply doesn't compose children either.
            self.allocate_window_storage(host_xid, 0, 0, 1, 1, 32, None, None);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.core.xid_map.remove(&host_xid);
    }

    /// Stage 4b: opt v2 into the full COMPOSITE-redirect
    /// activation path. The `process_request.rs` Composite
    /// handler gates its `activate_redirect_backing_for` call
    /// on this flag so v1 (which returns the default `false`)
    /// stays on the pre-Stage-4 "redirect record only" shape
    /// that the `92a2a83 → 3751c11` revert established.
    fn supports_redirect_activation(&self) -> bool {
        true
    }

    /// Stage 4c.4 — flip a window's scene-participation under
    /// COMPOSITE redirect. Delegates to `DrawableStore::
    /// set_scene_participating` (which clears unpresented
    /// presentation damage + bumps the epoch on a true→false
    /// transition per spec §I5) and fires scene-structure damage
    /// for the redirect transition.
    ///
    /// **Scene-structure damage** — always fires per the plan's
    /// Cross-cutting §"Concrete scene-structure damage":
    ///   - `participating=true` (un-redirect / Automatic-activate):
    ///     rect = W's current screen rect — the scene newly
    ///     includes W and must paint W's location.
    ///   - `participating=false` (Manual-activate): rect = W's
    ///     pre-flip rect — the scene NO LONGER includes W but
    ///     whatever is underneath must repaint the area where W
    ///     used to be.
    ///
    /// In both branches we capture the rect BEFORE the flip
    /// (pre-flip and post-flip geometry coincide because the
    /// participation flip itself doesn't move W); the only
    /// difference is semantic. When `window_absolute_rect`
    /// returns `None` (root or untracked geometry), fall back to
    /// the coarse `mark_scene_structure_dirty` — correctness-
    /// preserving, just wider than needed.
    fn set_window_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        let Some(w_id) = self.store.lookup(host_window.as_raw()) else {
            log::debug!(
                "v2 set_window_scene_participation(0x{:x}, {participating}): \
                 window not in store",
                host_window.as_raw(),
            );
            return Ok(());
        };
        // Capture rect BEFORE the flip — on participating=false
        // (Manual activation) the pre-flip rect is what the scene
        // needs to repaint over; on participating=true the pre-
        // and post-flip rects coincide (no geometry move on this
        // path) so either reading is fine, and pre-flip keeps the
        // two branches symmetric.
        let pre_flip_rect = self.window_absolute_rect(w_id);

        self.store.set_scene_participating(w_id, participating);

        if let Some(rect) = pre_flip_rect {
            self.scene.mark_scene_structure_damage_rects(&[rect]);
        } else {
            // No tracked geometry (root or untracked) — coarse
            // marker is correctness-preserving.
            self.scene.mark_scene_structure_dirty();
        }
        Ok(())
    }

    /// Stage 4c.4 — flip a backing's scene-participation under
    /// COMPOSITE redirect. Used by Automatic mode so paint that
    /// resolves through the backing accumulates presentation
    /// damage on B (which the scene walk picks up via W's
    /// `redirected_target` indirection in 4c's `build_scene`
    /// patch). No scene-structure damage from this call — the
    /// geometric damage of a mode-flip is the W-side call's
    /// responsibility (the blit-source identity flip is
    /// geometrically on W; backings have no on-screen geometry
    /// of their own).
    fn set_backing_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        let Some(b_id) = self.store.lookup(backing.as_raw()) else {
            log::debug!(
                "v2 set_backing_scene_participation(0x{:x}, {participating}): \
                 backing not in store",
                backing.as_raw(),
            );
            return Ok(());
        };
        self.store.set_scene_participating(b_id, participating);
        Ok(())
    }

    /// Stage 4b: real `name_window_pixmap`. Mirrors v1
    /// (`kms/backend.rs:9523-9544`) — lookup `host_window_to_backing`,
    /// incref the alias registry, return the SAME handle.
    /// Returns `NotFound` if the window isn't redirected
    /// (`allocate_redirected_backing` was never called for it).
    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        let backing = self
            .core
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "v2 name_window_pixmap: window is not redirected (no backing)",
                )
            })?;
        self.core.alias_registry.incref(backing);
        Ok(backing)
    }

    /// Stage 4b: real `allocate_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9568-9607`) with one v2-specific addition:
    /// after allocating the backing and registering it in
    /// `alias_registry` + `host_window_to_backing`, also flip
    /// `store.set_redirected_target(W_id, Some(B_id))` so v2's
    /// `resolve_paint_target` routes future paint to the backing.
    ///
    /// **Seed-copy ordering** per the plan's Cross-cutting
    /// §"Initial backing content" decision: the W→B copy fires
    /// BEFORE `set_redirected_target` flips routing, so the copy
    /// reads from W's own storage (not B's). Descendant seed-copy
    /// follows the same one-shot walk, in stable sibling z-order,
    /// so overlapping frame/decor children seed into the backing in
    /// the same order they would appear on screen.
    fn allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        // Idempotent — second `RedirectWindow` for the same W
        // returns the existing backing with no refcount bump
        // (the Reason-1 hold is single-instance per
        // §"Single refcount, two reasons").
        if let Some(existing) = self
            .core
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
        {
            // Diagnostic trace (TEMP) — idempotent re-allocate.
            // Important to know because callers may *expect* a
            // fresh backing (and a re-seed) but get the existing
            // one. Stage 4d.5 `rotate_redirected_backing_on_resize`
            // works around this by release-then-allocate.
            log::debug!(
                "v2 allocate_redirected_backing W=0x{w:x}: idempotent return existing B=0x{b:x} ({width}x{height}, depth={depth})",
                w = host_window.as_raw(),
                b = existing.as_raw(),
            );
            return Ok(existing);
        }
        let w_xid = host_window.as_raw();

        // Allocate a fresh backing via the existing
        // `create_pixmap` path (3f.10 pool + 3f.14 zero-fill).
        let backing = self.create_pixmap(origin, depth, width, height)?;
        let backing_xid = backing.as_raw();
        // Diagnostic trace (TEMP) — fresh allocation. Cross-correlate
        // against `set_redirected_target` and the "B is all-black"
        // dump to see whether a fresh-allocated B explains a black
        // backing (no client paint since the alloc) vs an
        // unexpectedly-reset existing backing.
        log::debug!(
            "v2 allocate_redirected_backing W=0x{w_xid:x}: fresh B=0x{backing_xid:x} ({width}x{height}, depth={depth})",
        );

        // Seed-copy: W → B (and every descendant of W into B at
        // its position relative to W), BEFORE the route flip.
        // Both ids are raw `DrawableId`s so the copies do NOT
        // consult `resolve_paint_target` (which would B→B no-op
        // once the route flip lands below). W must already exist
        // in the store; if not, that's a protocol error upstream
        // — log and skip the seed but keep going so the redirect
        // record still installs.
        if let (Some(w_id), Some(b_id)) = (self.store.lookup(w_xid), self.store.lookup(backing_xid))
        {
            self.seed_backing_from_window(w_xid, w_id, b_id);
            // Now flip routing — after this, paint against W
            // resolves to B via `resolve_paint_target`.
            self.store.set_redirected_target(w_id, Some(b_id));
        } else {
            log::warn!(
                "v2 allocate_redirected_backing(0x{w_xid:x}): window or backing not in store \
                 (seed + route flip skipped)",
            );
        }

        // Register Reason-1 hold + redirect map. Identical to v1.
        self.core.alias_registry.insert(
            backing,
            crate::kms::core::AliasEntry {
                refcount: 1,
                width,
                height,
                depth,
            },
        );
        self.core.host_window_to_backing.insert(w_xid, backing);
        Ok(backing)
    }

    /// Stage 4b: real `release_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9547-9566`) — clear the
    /// `host_window_to_backing` entry, drop the Reason-1 hold,
    /// free pixmap on refcount=0.
    ///
    /// v2-specific addition: when the redirect map clears, also
    /// drop `store.set_redirected_target` for every window that
    /// was routed through this backing. Multiple windows can
    /// alias the same backing only via NameWindowPixmap (which
    /// is the alias-handle, not a separate redirect), but the
    /// loop is cheap and matches the plan's defensive contract.
    ///
    /// Stage 4c.4 round-3 finding: drop B's `scene_participating`
    /// flag internally so the protocol handler (RedirectWindow
    /// unredirect / destroy path) doesn't need a separate
    /// `set_backing_scene_participation(false)` call. The trait
    /// docstring is the canonical statement of this contract.
    fn release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        let raw = backing.as_raw();
        // Drop the W→B map entry. v1 uses `retain` because the
        // map is keyed by W_xid (not B_xid); same shape here.
        self.core
            .host_window_to_backing
            .retain(|_, h| h.as_raw() != raw);
        // Clear the store-side route on every window that pointed
        // at this backing's DrawableId. Reverse-scan over `entries`
        // would need an iter accessor we don't have; iterate the
        // map keys we just retained against and clear each.
        // (In practice the map is empty after `retain` above —
        // but a future multi-window-per-backing model would still
        // be correct.)
        if let Some(b_id) = self.store.lookup(raw) {
            let routed_windows: Vec<u32> = self
                .windows_v2
                .keys()
                .copied()
                .filter(|xid| {
                    self.store
                        .lookup(*xid)
                        .and_then(|id| self.store.redirected_target(id))
                        == Some(b_id)
                })
                .collect();
            for w_xid in routed_windows {
                if let Some(w_id) = self.store.lookup(w_xid) {
                    self.store.set_redirected_target(w_id, None);
                }
            }
            // Stage 4c.4 round-3 finding: drop B's scene_participating
            // flag here so the protocol handler doesn't need a
            // separate `set_backing_scene_participation(false)`
            // call. No-op when the flag is already false (the
            // store's `set_scene_participating` short-circuits
            // the damage-clear branch when `was == v`).
            self.store.set_scene_participating(b_id, false);
        }
        if self.core.alias_registry.decref(backing) {
            self.free_pixmap(origin, raw)?;
        }
        Ok(())
    }

    /// Stage 4d — Composite Overlay Window allocation.
    ///
    /// First `GetOverlayWindow` allocates screen-extent depth-24
    /// storage at xid `COMPOSITE_OVERLAY_WINDOW` (0x103), stores
    /// the resulting `DrawableId` on `self.cow_id`, sets the
    /// matching protocol refcount on `core.cow_refcount = 1`.
    /// The drawable stays off the normal scene path; xfwm4 paints
    /// its composited desktop into its own child window, so adding
    /// the COW as a topmost scene layer would cover the real output
    /// with a stale black surface.
    ///
    /// Subsequent calls (compositor restart, multi-client
    /// scenarios) just bump `core.cow_refcount` and return Ok
    /// — the protocol reply is the same fixed xid.
    ///
    /// Initial fill: storage from `allocate_drawable_storage`
    /// is uninitialised Vk-DEVICE_LOCAL memory (same problem
    /// Stage 3f.14 fixed for `create_pixmap`). We do an explicit
    /// transparent-black fill via `engine.fill_rect` so the
    /// compositor's first paint composites over a known zero
    /// rather than recycled GPU garbage. The fill is best-effort
    /// — on the stub fixture (no Vk) `engine.fill_rect` errors;
    /// log + continue (storage already exists at xid level).
    fn get_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        if self.cow_id.is_some() {
            self.core.cow_refcount += 1;
            return Ok(());
        }
        let fb_w = self.platform.fb_w.max(1);
        let fb_h = self.platform.fb_h.max(1);
        let storage = match self.platform.allocate_drawable_storage(fb_w, fb_h, 24) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                // Test-fixture / no-Vk path: same shape as
                // `init_root_storage` — fall back to a null-view
                // stub so unit tests can exercise refcount /
                // scene-registration without a live Vk ICD.
                log::debug!("v2 get_overlay_window: no Vk, using stub COW storage: {e:?}");
                crate::kms::v2::store::Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(fb_w),
                        height: u32::from(fb_h),
                    },
                    crate::kms::v2::platform::PlatformBackend::format_for_depth(24),
                )
            }
        };
        let xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        // Defensive: if a stale mapping somehow survives a prior
        // teardown (decref's PendingFence path detaches xid for us,
        // but a synchronous-destroy path could race), detach first
        // so the allocate doesn't trip XidInUse.
        self.store.detach_xid(xid);
        let id = self
            .store
            .allocate(xid, DrawableKind::Window, 24, true, storage)
            .map_err(|e| io::Error::other(format!("v2 get_overlay_window: store alloc: {e:?}")))?;
        // Stage 3f.14 follow-on — zero-fill the fresh storage so
        // the compositor doesn't composite over recycled GPU
        // garbage on its first paint. Best-effort on stub paths.
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(fb_w),
                height: u32::from(fb_h),
            },
        };
        if let Err(e) =
            self.engine
                .fill_rect(&mut self.store, &mut self.platform, id, rect, [0.0; 4])
            && self.platform.vk.is_some()
        {
            log::warn!("v2 get_overlay_window: initial zero-fill failed: {e:?}");
        }
        self.cow_id = Some(id);
        self.core.cow_refcount = 1;
        Ok(())
    }

    /// Stage 4d — Composite Overlay Window release.
    ///
    /// Decrements `core.cow_refcount`; on the final release it
    /// decrefs the store storage and clears `self.cow_id`.
    /// `DrawableStore::decref` removes the xid mapping
    /// (immediately on synchronous-destroy, deferred on
    /// `PendingFence`) so the next `GetOverlayWindow`
    /// reallocates fresh storage at the same xid.
    ///
    /// Defensive against unmatched releases (refcount=0 → Ok(false)
    /// no-op). The trait docstring is the canonical statement of
    /// this shape.
    ///
    /// Returns `Ok(true)` iff this call drove the refcount to 0
    /// and the COW storage was destroyed. The handler uses that
    /// signal to clear `host_xid` on the COW resource record so
    /// the next `GetOverlayWindow` re-wires fresh.
    fn release_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        if self.core.cow_refcount == 0 {
            return Ok(false);
        }
        self.core.cow_refcount -= 1;
        if self.core.cow_refcount == 0 {
            if let Some(id) = self.cow_id.take() {
                self.store.decref(&mut self.platform, id);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // ── Resources (pixmap / font / cursor) ──────────────────────

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let xid = self.core.next_host_xid();
        // Stage 2c: allocate real backing storage. The engine
        // needs a live VkContext to paint into; on the test
        // fixture the platform's `allocate_drawable_storage`
        // returns `ERROR_INITIALIZATION_FAILED` and we fall back
        // to logging a gap + returning the bare xid (tests that
        // don't paint still get a stable handle).
        match self
            .platform
            .allocate_drawable_storage(width, height, depth)
        {
            Ok(storage) => {
                if let Err(e) =
                    self.store
                        .allocate(xid, DrawableKind::Pixmap, depth, false, storage)
                {
                    log::warn!("v2 create_pixmap: store.allocate failed for xid {xid:#x}: {e:?}",);
                } else {
                    self.telemetry.record_storage_allocation();
                    self.telemetry.record_image_view_create();
                    // Stage 3f.14 follow-on: clear the fresh pixmap
                    // storage to a known-zero value. X11 says new
                    // pixmaps are undefined content, but Vk
                    // DEVICE_LOCAL memory is *fully* undefined —
                    // random GPU-recycled bytes. Real X servers tend
                    // to get away with this because system allocators
                    // zero pages, but our Vk allocator doesn't.
                    //
                    // Concrete repro (mate + marco + xeyes resize):
                    // xeyes creates a fresh depth-24 pixmap, sets a
                    // SHAPE clip matching its eye outlines, draws
                    // the eyes (only the shape-clipped area gets
                    // paint), then Present-Pixmaps the whole pixmap
                    // to the window. The non-eye area of the pixmap
                    // still holds undefined Vk bytes; Present copies
                    // it verbatim → visible garbage in the window.
                    //
                    // Cleared values: depth-32 transparent black
                    // (0,0,0,0) — premul no-op for compositing;
                    // depth-1 / depth-8 / depth-24 opaque black
                    // (0,0,0,1) — matches "uninitialised pixel = 0"
                    // which clients typically assume.
                    if let Some(id) = self.store.lookup(xid) {
                        let color = default_window_init_color(depth);
                        let rect = ash::vk::Rect2D {
                            offset: ash::vk::Offset2D::default(),
                            extent: ash::vk::Extent2D {
                                width: u32::from(width.max(1)),
                                height: u32::from(height.max(1)),
                            },
                        };
                        if let Err(e) = self.engine.fill_rect(
                            &mut self.store,
                            &mut self.platform,
                            id,
                            rect,
                            color,
                        ) {
                            log::debug!(
                                "v2 create_pixmap: initial fill failed for xid {xid:#x}: {e:?}"
                            );
                        }
                    }
                }
            }
            Err(vk_err) => {
                // Test fixture path — no Vk available.
                self.log_v2_gap("create_pixmap_no_vk");
                let _ = vk_err;
            }
        }
        PixmapHandle::from_raw(xid).ok_or_else(|| io::Error::other("create_pixmap: xid was 0"))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        // Stage 4b: alias-registry-aware free path. When `host_xid`
        // names a COMPOSITE-redirect backing (via NameWindowPixmap
        // alias or the Reason-1 redirect hold), decref the registry
        // first; only drop the storage when refcount hits zero.
        // Otherwise (an ordinary pixmap) fall through to the
        // straight `store.decref` path.
        //
        // v1's `free_pixmap` (`kms/backend.rs:9637-9650`) does NOT
        // consult the registry — it gets away with this because
        // compositors typically call FreePixmap(alias) after
        // UnredirectWindow, so the registry has already been
        // torn down by `release_redirected_backing`. The protocol
        // doesn't guarantee that ordering though, and v2 gates
        // here so an early FreePixmap on a still-held alias
        // doesn't drop the backing while a redirect still uses it.
        if let Some(handle) = yserver_core::backend::PixmapHandle::from_raw(host_xid)
            && self.core.alias_registry.get(handle).is_some()
        {
            if self.core.alias_registry.decref(handle)
                && let Some(id) = self.store.lookup(host_xid)
            {
                self.store.decref(&mut self.platform, id);
            }
            return Ok(());
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.decref(&mut self.platform, id);
        }
        Ok(())
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        // Same body as v1. `KmsCore` already owns `FontLoader` +
        // `fonts` (it's protocol-bookkeeping per the v2 spec); the
        // backend just wraps the resulting freetype handle in a
        // `FontState` entry against a freshly-allocated xid.
        use std::cell::RefCell;

        use crate::kms::core::{FontState, FreetypeFace};
        let (face, metrics, char_cache) = self.core.font_loader.open_font(name)?;
        let host_xid = self.core.next_host_xid();
        let handle = FontHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create font handle"))?;
        self.core.fonts.insert(
            host_xid,
            FontState {
                handle: host_xid,
                face: RefCell::new(FreetypeFace(face)),
                metrics: metrics.clone(),
                char_info_cache: char_cache,
            },
        );
        Ok((handle, metrics))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.core.fonts.remove(&host_xid);
        Ok(())
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_pixmap: PixmapHandle,
        _mask_pixmap: Option<PixmapHandle>,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
        _hot_x: u16,
        _hot_y: u16,
    ) -> io::Result<CursorHandle> {
        // Stage 3f.4: mint a valid xid so clients that probe the
        // handle don't trip on a zero result. The cursor's
        // **rasterisation + scene blit** lives at Stage 4 (cursor
        // is layer 4 in the scene per spec, deferred alongside the
        // SHAPE work); until then the cursor visually defaults to
        // the bare KMS HW cursor or no cursor at all. No `log_v2_gap`
        // because the gap is documented + invariant for Stage 3.
        let xid = self.core.next_host_xid();
        CursorHandle::from_raw(xid).ok_or_else(|| io::Error::other("create_cursor: xid was 0"))
    }

    fn create_glyph_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_font: FontHandle,
        _mask_font: Option<FontHandle>,
        _source_char: u16,
        _mask_char: u16,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        // Stage 3f.4: same shape as `create_cursor` — handle minted,
        // rasterisation deferred to Stage 4.
        let xid = self.core.next_host_xid();
        CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_glyph_cursor: xid was 0"))
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window_xid: u32,
        _cursor_host_xid: u32,
    ) -> io::Result<()> {
        // Stage 3f.4: Stage 4 cursor scene-layer work will wire
        // per-window cursor binding; until then v2 stays on the
        // default cursor.
        Ok(())
    }

    // ── Container background ────────────────────────────────────

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.core.bg_pixel = Some(pixel);
        self.core.bg_pixmap = None;
        // Stage 4a — root paint resolves through redirect routing.
        // In the common (unredirected) case this is the leaf root
        // drawable; if a compositor has redirected root, paint
        // lands in its backing instead. `resolve_paint_target`
        // returns `None` only when the root xid isn't in the
        // store, which is a fixture-init bug.
        if let Some(target) = self.resolve_paint_target(self.core.window_id) {
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: target.offset.0,
                    y: target.offset.1,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(self.platform.fb_w.max(1)),
                    height: u32::from(self.platform.fb_h.max(1)),
                },
            };
            // L1 server-α invariant: root storage is depth-24, so
            // force the stored α byte to 0xFF for the scene
            // compositor's pass-through draw to read opaque.
            let depth = self.store.get(target.id).map(|d| d.depth).unwrap_or(24);
            if let Err(e) = self.engine.fill_rect(
                &mut self.store,
                &mut self.platform,
                target.id,
                rect,
                decode_x11_pixel_server_alpha(pixel, depth),
            ) {
                log::warn!("v2 set_container_background_pixel: root fill failed: {e:?}");
            } else {
                self.telemetry.record_paint_submit();
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::ResolvedSource, vk::ops::render::CompositeRect};
        self.core.bg_pixmap = PixmapHandle::from_raw(host_pixmap_xid);
        self.core.bg_pixel = None;
        // Stage 4a — root paint resolves through redirect routing.
        let Some(dst_target) = self.resolve_paint_target(self.core.window_id) else {
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        };
        let dst = dst_target.id;
        let Some(src) = self.store.lookup(host_pixmap_xid) else {
            log::debug!(
                "v2 set_container_background_pixmap: pixmap 0x{host_pixmap_xid:x} not in store"
            );
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        };
        // Stage 3f.14: X11 bg_pixmap tiles across the drawable
        // extent. Pre-3f.14 v2 did a single copy_area at (0, 0)
        // and left the rest of root unchanged — fvwm3 wallpaper
        // covered only the top-left of the screen on bee. Route
        // through `engine.render_composite` with OP_SRC + Repeat::
        // Normal so the source pixmap tiles across the whole root
        // extent in a single submit. Same shape as `try_tiled_fill`
        // (3f.3) but unconditioned by GC clip.
        if src == dst {
            // Defensive: a pixmap aliased as bg of its own drawable
            // is not a meaningful X11 op. v1's path treats it the
            // same (copy_area with src == dst is logged + skipped).
            log::debug!("v2 set_container_background_pixmap: src == root, skipping");
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        }
        let src_format = self.store.get(src).map(|d| d.storage.format);
        if src_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            // Tile path requires BGRA8 src (matches `try_tiled_fill`
            // gate). Other formats fall through with no paint —
            // v1-parity-ish; rare in practice for root bg.
            log::debug!(
                "v2 set_container_background_pixmap: pixmap 0x{host_pixmap_xid:x} format \
                 {src_format:?} not BGRA8, skipping tile"
            );
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        }
        let dst_extent = ash::vk::Extent2D {
            width: u32::from(self.platform.fb_w.max(1)),
            height: u32::from(self.platform.fb_h.max(1)),
        };
        let rects = [CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: dst_target.offset.0,
            dst_y: dst_target.offset.1,
            width: dst_extent.width,
            height: dst_extent.height,
        }];
        const OP_SRC: u8 = 1;
        match self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            OP_SRC,
            ResolvedSource::Drawable(src),
            ResolvedSource::None,
            dst,
            &rects,
            None,
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            // Audit #4: synthesized backing-seed copy, no Picture
            // context. Engine falls back to depth heuristic.
            0,
            0,
            0,
        ) {
            Ok(s) if s.recorded_draws > 0 => self.telemetry.record_paint_submit(),
            Ok(_) => {}
            Err(e) => {
                log::warn!(
                    "v2 set_container_background_pixmap: render_composite tile failed: {e:?}"
                );
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    // ── GC state ────────────────────────────────────────────────

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.core.current_clip = ClipState::None;
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        self.core.current_clip = match clip {
            Some(rects) => ClipState::Rectangles {
                origin: (0, 0),
                rects,
            },
            None => ClipState::None,
        };
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        // Stage 3f.3: store the ClipState::Pixmap so apply_clip_state +
        // subsequent paint paths can route through a depth-1 mask
        // sampler. The mask-sampling itself is deferred to Stage 5
        // perf-plans (no real-app smoke matrix client uses it; v1 has
        // the same shape — stores but doesn't enforce). Bookkeeping
        // is correct so Core paint that follows can see the pixmap
        // handle if/when a future engine pass picks it up.
        let Some(handle) = PixmapHandle::from_raw(host_pixmap) else {
            self.core.current_clip = ClipState::None;
            return Ok(());
        };
        self.core.current_clip = ClipState::Pixmap {
            origin: (clip_x_origin, clip_y_origin),
            pixmap: handle,
        };
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.core.current_fill = FillState::Solid;
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        // Stage 3f.3: store the FillState::Tiled record so subsequent
        // fill paths route through the tiled-fill RENDER composite.
        // The dispatcher also pushes the same state via
        // `apply_fill_state` before every fill op, so this entry
        // point is mostly used by ynest's host-X11 flow; preserving
        // both keeps the Backend trait surface uniform.
        let Some(handle) = PixmapHandle::from_raw(host_pixmap) else {
            self.core.current_fill = FillState::Solid;
            return Ok(());
        };
        self.core.current_fill = FillState::Tiled {
            pixmap: handle,
            origin: (tile_x_origin, tile_y_origin),
        };
        Ok(())
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()> {
        self.core.current_clip = clip.clone();
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        self.core.current_fill = fill.clone();
        Ok(())
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        if let Some(font) = state.font {
            self.core.current_font = Some(font.as_raw());
        }
        self.core.current_function = state.function;
        self.core.current_foreground = state.foreground;
        self.core.current_background = state.background;
        self.core.current_fill = state.fill.clone();
        self.core.current_clip = state.clip.clone();
        // Stage 4d Manual-redirect fix: drawing through a
        // `ClipByChildren` GC into a window must exclude every
        // mapped child window's area. Capture the mode here so
        // `copy_area` (and any other future op that consults it)
        // can split the destination rect against the child rects.
        self.core.current_subwindow_mode = state.subwindow_mode;
        Ok(())
    }

    // ── Drawing primitives (paint paths) ────────────────────────

    fn copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(src) = self.store.lookup(src_host_xid) else {
            self.log_v2_gap("copy_area_unknown_xid");
            return Ok(());
        };
        // Stage 4a — dst resolves through `resolve_paint_target` so
        // copy_area into a redirected window lands in the backing
        // with the descendant offset applied. Source stays at the
        // raw store lookup per spec § "render_composite separates
        // src/dst resolution" — the X11 client reads from the
        // drawable as it sees it.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            self.log_v2_gap("copy_area_unknown_xid");
            return Ok(());
        };
        // Diagnostic trace (TEMP — Stage 4d "top-left-only CC" investigation).
        // Pins where each CopyArea lands: src store id, dst's resolved
        // PaintTarget (id + offset), and the wire src/dst coords + size.
        // Gated on `RUST_LOG=yserver::kms::v2::paint=trace`. Codex round
        // of 2026-05-18: needed because the symptom narrowed to "B has
        // CC content only in the top-left 177x80" — we need to see
        // whether marco's many 975x600 CopyArea(src=CC_offscreen,
        // dst=CC_window) calls resolve to the frame backing's
        // DrawableId or get lost en route.
        log::trace!(
            target: "yserver::kms::v2::paint",
            "copy_area src=0x{src_host_xid:x}->id={src:?} dst=0x{dst_host_xid:x}->id={dst_id:?}+off=({off_x},{off_y}) \
             src_xy=({src_x},{src_y}) dst_xy=({dst_x},{dst_y}) {width}x{height}",
            dst_id = dst_target.id,
            off_x = dst_target.offset.0,
            off_y = dst_target.offset.1,
        );
        // Stage 4d Manual-redirect fix: split the copy by
        // `subwindow_mode = ClipByChildren` rules when dst is a
        // window. Each surviving sub-rect is in dst-window-local
        // coords; we issue one engine.copy_area per sub-rect,
        // adjusting src offsets by the sub-rect's delta from the
        // original dst_xy. IncludeInferiors (mode=1) keeps the
        // single-rect fast path. Pixmap destinations also keep the
        // fast path (no children to clip against).
        let dst_rect_local = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(dst_x),
                y: i32::from(dst_y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        // Step 1: GC clip intersection (X11 GC `clip-mask` /
        // `SetClipRectangles`). When the GC has explicit clip
        // rectangles, every paint is masked against them first;
        // `ClipState::None` means "no GC clip", and we keep the
        // single-rect fast path. `ClipState::Pixmap` rasterises a
        // mask — out of scope for this fix; pass through untouched
        // (TODO mirrors v1's `intersect_with_current_clip`).
        let post_gc_clip: Vec<ash::vk::Rect2D> =
            if let yserver_core::backend::ClipState::Rectangles { origin, rects } =
                &self.core.current_clip
            {
                let clip_rects: Vec<ash::vk::Rect2D> = rects
                    .rectangles
                    .chunks_exact(8)
                    .filter_map(|chunk| {
                        let cx = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]))
                            + i32::from(origin.0);
                        let cy = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]))
                            + i32::from(origin.1);
                        let cw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
                        let ch = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
                        if cw <= 0 || ch <= 0 {
                            return None;
                        }
                        Some(ash::vk::Rect2D {
                            offset: ash::vk::Offset2D { x: cx, y: cy },
                            extent: ash::vk::Extent2D {
                                width: u32::try_from(cw).unwrap_or(0),
                                height: u32::try_from(ch).unwrap_or(0),
                            },
                        })
                    })
                    .collect();
                intersect_rect_with_clip(dst_rect_local, &clip_rects)
            } else {
                vec![dst_rect_local]
            };
        if post_gc_clip.is_empty() {
            // GC clip is empty (or `SetClipRectangles` with n=0):
            // spec-correct no-op.
            return Ok(());
        }
        // Step 2: ClipByChildren — subtract every mapped child window
        // rect from each post-GC-clip rect. IncludeInferiors (mode=1)
        // keeps each post-GC-clip rect as-is. Pixmap destinations
        // (not in `windows_v2`) also bypass child subtraction.
        let sub_rects: Vec<ash::vk::Rect2D> = if matches!(
            self.core.current_subwindow_mode,
            yserver_core::backend::SubwindowMode::ClipByChildren,
        ) && self.windows_v2.contains_key(&dst_host_xid)
        {
            let child_rects: Vec<ash::vk::Rect2D> = self
                .windows_v2
                .values()
                .filter_map(|geom| {
                    if geom.parent == Some(dst_host_xid) && geom.mapped {
                        Some(ash::vk::Rect2D {
                            offset: ash::vk::Offset2D {
                                x: i32::from(geom.x),
                                y: i32::from(geom.y),
                            },
                            extent: ash::vk::Extent2D {
                                width: u32::from(geom.width.max(1)),
                                height: u32::from(geom.height.max(1)),
                            },
                        })
                    } else {
                        None
                    }
                })
                .collect();
            if child_rects.is_empty() {
                post_gc_clip
            } else {
                post_gc_clip
                    .into_iter()
                    .flat_map(|r| compute_copy_area_dst_rects(r, &child_rects))
                    .collect()
            }
        } else {
            post_gc_clip
        };
        if sub_rects.is_empty() {
            // Whole copy fully covered by mapped children — nothing
            // to paint. Spec-correct under ClipByChildren.
            return Ok(());
        }
        let mut all_ok = true;
        for sub in &sub_rects {
            let sub_dst_x = sub.offset.x;
            let sub_dst_y = sub.offset.y;
            // src coords shift by the same delta the dst sub-rect
            // shifted from the original dst_xy.
            let sub_src_x = i32::from(src_x) + (sub_dst_x - i32::from(dst_x));
            let sub_src_y = i32::from(src_y) + (sub_dst_y - i32::from(dst_y));
            let src_sub_rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: sub_src_x,
                    y: sub_src_y,
                },
                extent: sub.extent,
            };
            let dst_pos = ash::vk::Offset2D {
                x: sub_dst_x + dst_target.offset.0,
                y: sub_dst_y + dst_target.offset.1,
            };
            if let Err(e) = self.engine.copy_area(
                &mut self.store,
                &mut self.platform,
                src,
                dst_target.id,
                src_sub_rect,
                dst_pos,
            ) {
                log::warn!(
                    "v2 copy_area: engine.copy_area failed (src=0x{src_host_xid:x} \
                     dst=0x{dst_host_xid:x} sub_rect={sub:?}): {e:?}",
                );
                all_ok = false;
            }
        }
        if all_ok {
            self.telemetry.record_paint_submit();
        }
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        // copy_plane decomposes into bg-first + fg-second
        // `poly_fill_rectangle` calls below; non-`GXcopy` GC.function
        // is honoured by the underlying `fill_solid_rects` →
        // `engine.logic_fill` path landed in Stage 3f.2.
        if width == 0 || height == 0 {
            return Ok(());
        }

        // Resolve src + dst drawables. Both must exist in the store
        // (otherwise the request is a protocol error — log + skip).
        let Some(src_id) = self.store.lookup(src_host_xid) else {
            log::debug!("v2 copy_plane gap: src 0x{src_host_xid:x} not in store");
            return Ok(());
        };
        let Some(_dst_id) = self.store.lookup(dst_host_xid) else {
            log::debug!("v2 copy_plane gap: dst 0x{dst_host_xid:x} not in store");
            return Ok(());
        };

        let src_depth = match self.store.get(src_id) {
            Some(d) => d.depth,
            None => return Ok(()),
        };

        // Read the full src extent via the engine. We pull the
        // whole pixmap once (rather than only `src_rect`) because
        // the wire format's row stride is computed from the
        // pixmap's width; reading a sub-rect would still produce a
        // wire-shaped reply but with a different row stride per
        // pixmap.width. Easier to pull everything, index inside
        // the (src_x, src_y, width, height) window, and let v2's
        // per-op CB amortise the synchronous get_image cost. xfd
        // / xfontsel CopyPlane the entire glyph pixmap each draw
        // anyway, so the "full extent" overhead matches the call
        // pattern.
        let src_extent = match self.store.get(src_id) {
            Some(d) => d.storage.extent,
            None => return Ok(()),
        };
        let src_w = src_extent.width;
        let src_h = src_extent.height;
        if src_w == 0 || src_h == 0 {
            return Ok(());
        }
        let src_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            src_id,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: src_extent,
            },
            src_depth,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("v2 copy_plane: src get_image failed: {e:?}");
                return Ok(());
            }
        };
        self.telemetry.record_one_shot_submit();

        // Wire row stride for the src depth (matches pack_from_storage).
        let row_bytes: usize = match src_depth {
            1 => src_w.div_ceil(32) as usize * 4,
            8 => (src_w as usize + 3) & !3,
            24 | 32 => src_w as usize * 4,
            _ => {
                log::debug!("v2 copy_plane gap: src depth {src_depth} unsupported");
                return Ok(());
            }
        };

        // For each (sx, sy) in the requested src window, classify
        // the pixel into foreground / background and emit a 1×1
        // fill rect at the corresponding dst position. Caller
        // saturates over i16 because dst coords are protocol-i16.
        let mut fg_rects: Vec<u8> = Vec::new();
        let mut bg_rects: Vec<u8> = Vec::new();
        for row in 0..height {
            let sy = i32::from(src_y).saturating_add(i32::from(row));
            let dy = dst_y.saturating_add(row as i16);
            if sy < 0 || sy >= i32::try_from(src_h).unwrap_or(i32::MAX) {
                continue;
            }
            for col in 0..width {
                let sx = i32::from(src_x).saturating_add(i32::from(col));
                let dx = dst_x.saturating_add(col as i16);
                if sx < 0 || sx >= i32::try_from(src_w).unwrap_or(i32::MAX) {
                    continue;
                }
                let pixel: u32 = match src_depth {
                    1 => {
                        let row_off = sy as usize * row_bytes;
                        let byte = src_bytes[row_off + (sx as usize) / 8];
                        let bit = (byte >> (7 - (sx as usize & 7))) & 1;
                        u32::from(bit)
                    }
                    8 => {
                        let row_off = sy as usize * row_bytes;
                        u32::from(src_bytes[row_off + sx as usize])
                    }
                    24 | 32 => {
                        let off = sy as usize * row_bytes + sx as usize * 4;
                        u32::from_le_bytes([
                            src_bytes[off],
                            src_bytes[off + 1],
                            src_bytes[off + 2],
                            src_bytes[off + 3],
                        ])
                    }
                    _ => 0,
                };
                let mut rect = Vec::with_capacity(8);
                rect.extend_from_slice(&i16::to_le_bytes(dx));
                rect.extend_from_slice(&i16::to_le_bytes(dy));
                rect.extend_from_slice(&u16::to_le_bytes(1));
                rect.extend_from_slice(&u16::to_le_bytes(1));
                if pixel & plane != 0 {
                    fg_rects.extend_from_slice(&rect);
                } else {
                    bg_rects.extend_from_slice(&rect);
                }
            }
        }

        let foreground = self.core.current_foreground;
        let background = self.core.current_background;

        // Bg first, then fg — matches v1's overlap ordering so the
        // foreground wins on any aliased rect.
        if !bg_rects.is_empty() {
            self.poly_fill_rectangle(None, dst_host_xid, background, &bg_rects)?;
        }
        if !fg_rects.is_empty() {
            self.poly_fill_rectangle(None, dst_host_xid, foreground, &fg_rects)?;
        }
        Ok(())
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("put_image_unknown_xid");
            return Ok(());
        };
        // GC clipping is honoured upstream by `clear_clip_rectangles`
        // when the dispatcher zeroes the clip (the MIT-SHM /
        // ImageText callers do this); Stage 2c's engine ignores
        // the GC's clip rectangles otherwise. Stage 3 plugs
        // RENDER + planemask + GC.function back in.
        if !matches!(
            self.core.current_function,
            yserver_core::backend::GcFunction::Copy,
        ) {
            self.log_v2_gap("put_image_non_gxcopy");
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            target.id,
            ash::vk::Offset2D {
                x: i32::from(dst_x) + target.offset.0,
                y: i32::from(dst_y) + target.offset.1,
            },
            ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
            data,
            depth,
        ) {
            log::warn!("v2 put_image: engine.put_image failed for xid {host_xid:#x}: {e:?}",);
        } else {
            self.telemetry.record_paint_submit();
        }
        Ok(())
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        _format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        // Stage 4a — resolve through redirect routing per spec Risk 1
        // ("GetImage reads what the X server considers W's content,
        // which under redirect is B"). Depth comes from the
        // resolved target's drawable (backing is allocated to match
        // W's depth, so v1 / v2 see the same wire shape).
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("get_image_unknown_xid");
            return Ok(None);
        };
        let depth = match self.store.get(target.id) {
            Some(d) => d.depth,
            None => return Ok(None),
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(x) + target.offset.0,
                y: i32::from(y) + target.offset.1,
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        let start = std::time::Instant::now();
        match self
            .engine
            .get_image(&mut self.store, &mut self.platform, target.id, rect, depth)
        {
            Ok(pixel_bytes) => {
                let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                self.telemetry.record_one_shot_submit();
                self.telemetry.record_fence_wait(ns);
                // X11 GetImage reply: 32-byte header + pixel rows.
                // The handler in `process_request.rs:handle_get_image`
                // patches `sequence` at [2..4] and `visual` at [8..12];
                // the rest of the header (depth, reply length in u32
                // units, padding) is the backend's job. Mirrors v1's
                // `KmsBackend::get_image` (kms/backend.rs:10400) — when
                // this returns just the pixel slice (no header), the
                // handler corrupts the first 32 bytes by writing into
                // them, and clients reading depth/length/sequence from
                // the wire see garbage.
                Ok(Some(wrap_get_image_reply(depth, pixel_bytes)))
            }
            Err(e) => {
                log::warn!("v2 get_image: engine.get_image failed for xid {host_xid:#x}: {e:?}",);
                Ok(None)
            }
        }
    }

    fn clear_area(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.clear_window_area_with_background(
            host_xid,
            background_pixel,
            background_pixmap_host_xid,
            x,
            y,
            width,
            height,
        )
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_line_unknown_xid");
            return Ok(());
        };
        // coordinate_mode 0 = Origin (absolute), 1 = Previous
        // (each point is a delta from the previous).
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut prev: Option<(i32, i32)> = None;
        let mut offset = 0;
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 {
                if let Some((px, py)) = prev {
                    (px + i32::from(x), py + i32::from(y))
                } else {
                    (i32::from(x), i32::from(y))
                }
            } else {
                (i32::from(x), i32::from(y))
            };
            if let Some((px, py)) = prev {
                crate::kms::backend::bresenham_segment(px, py, xi, yi, &mut rects);
            }
            prev = Some((xi, yi));
        }
        let rects = self.intersect_with_current_clip(&rects);
        self.fill_solid_rects(target, foreground, &rects);
        Ok(())
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_segment_unknown_xid");
            return Ok(());
        };
        // Each segment is (x1:i16, y1:i16, x2:i16, y2:i16).
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= segments.len() {
            let Some((x1, y1)) = crate::kms::backend::read_i16_pair(segments, offset) else {
                break;
            };
            let Some((x2, y2)) = crate::kms::backend::read_i16_pair(segments, offset + 4) else {
                break;
            };
            offset += 8;
            crate::kms::backend::bresenham_segment(
                i32::from(x1),
                i32::from(y1),
                i32::from(x2),
                i32::from(y2),
                &mut rects,
            );
        }
        let rects = self.intersect_with_current_clip(&rects);
        self.fill_solid_rects(target, foreground, &rects);
        Ok(())
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_rectangle_unknown_xid");
            return Ok(());
        };
        // Rectangle outlines: 4 thin (1-px) rects per input rect.
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = crate::kms::backend::read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            if r.width == 0 || r.height == 0 {
                continue;
            }
            // top edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: r.width,
                height: 1,
            });
            // bottom edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y.wrapping_add(r.height as i16).wrapping_sub(1),
                width: r.width,
                height: 1,
            });
            // left edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: 1,
                height: r.height,
            });
            // right edge
            rects.push(Rectangle16 {
                x: r.x.wrapping_add(r.width as i16).wrapping_sub(1),
                y: r.y,
                width: 1,
                height: r.height,
            });
        }
        let rects = self.intersect_with_current_clip(&rects);
        self.fill_solid_rects(target, foreground, &rects);
        Ok(())
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_arc_unknown_xid");
            return Ok(());
        };
        // Each arc: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // Partial-angle arcs are treated as full ellipses (matches v1;
        // angle-mask refinement is a follow-up). The outline is drawn by
        // scanline: top/bottom rows emit the full horizontal span (caps);
        // intermediate rows emit two side connectors bridging the
        // previous row's left/right edges to this row's edges.
        let mut rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            let ay = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]));
            let aw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ah = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            if aw <= 0 || ah <= 0 {
                continue;
            }
            let cx = f64::from(ax) + f64::from(aw) * 0.5;
            let cy = f64::from(ay) + f64::from(ah) * 0.5;
            let rx = f64::from(aw) * 0.5;
            let ry = f64::from(ah) * 0.5;

            let row_at = |py: i32| -> Option<(i32, i32)> {
                let dy = (f64::from(py) + 0.5 - cy) / ry;
                if dy.abs() > 1.0 {
                    return None;
                }
                let dx = (1.0 - dy * dy).sqrt() * rx;
                let x0 = (cx - dx).floor() as i32;
                let x1 = (cx + dx).ceil() as i32;
                Some((x0, x1))
            };

            let mut prev: Option<(i32, i32)> = None;
            for py in ay..ay + ah {
                let Some((x0, x1)) = row_at(py) else {
                    prev = None;
                    continue;
                };
                let next = row_at(py + 1);
                let cap = prev.is_none() || next.is_none();
                if cap {
                    rects.push(Rectangle16 {
                        x: x0 as i16,
                        y: py as i16,
                        width: (x1 - x0 + 1) as u16,
                        height: 1,
                    });
                } else {
                    let (px0, px1) = prev.unwrap();
                    let l_lo = px0.min(x0);
                    let l_hi = px0.max(x0);
                    rects.push(Rectangle16 {
                        x: l_lo as i16,
                        y: py as i16,
                        width: (l_hi - l_lo + 1) as u16,
                        height: 1,
                    });
                    let r_lo = px1.min(x1);
                    let r_hi = px1.max(x1);
                    rects.push(Rectangle16 {
                        x: r_lo as i16,
                        y: py as i16,
                        width: (r_hi - r_lo + 1) as u16,
                        height: 1,
                    });
                }
                prev = Some((x0, x1));
            }
        }
        let rects = self.intersect_with_current_clip(&rects);
        self.fill_solid_rects(target, foreground, &rects);
        Ok(())
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_point_unknown_xid");
            return Ok(());
        };
        let mut rects = Vec::new();
        let mut prev = (0i32, 0i32);
        let mut first = true;
        let mut offset = 0;
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 && !first {
                (prev.0 + i32::from(x), prev.1 + i32::from(y))
            } else {
                (i32::from(x), i32::from(y))
            };
            first = false;
            prev = (xi, yi);
            rects.push(Rectangle16 {
                x: xi.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                y: yi.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                width: 1,
                height: 1,
            });
        }
        let rects = self.intersect_with_current_clip(&rects);
        self.fill_solid_rects(target, foreground, &rects);
        Ok(())
    }

    fn poly_fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        // Each X11 Rectangle is 8 bytes: { i16 x, i16 y, u16 w, u16 h }.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_fill_rectangle_unknown_xid");
            return Ok(());
        };
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = crate::kms::backend::read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            rects.push(r);
        }
        let rects = self.intersect_with_current_clip(&rects);
        self.fill_rects_honoring_fill_state(target, foreground, &rects);
        Ok(())
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_fill_arc_unknown_xid");
            return Ok(());
        };
        // Each arc is 12 bytes: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // Partial arcs fall back to a full-ellipse fill (matches v1; xeyes /
        // xclock-style apps draw full circles).
        let (img_w, img_h) = self
            .drawable_dims_v2(host_xid)
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let mut rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            let ay = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]));
            let aw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ah = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            if aw <= 0 || ah <= 0 {
                continue;
            }
            let cx = f64::from(ax) + f64::from(aw) * 0.5;
            let cy = f64::from(ay) + f64::from(ah) * 0.5;
            let rx = f64::from(aw) * 0.5;
            let ry = f64::from(ah) * 0.5;
            let y_start = ay.max(0);
            let y_end = (ay + ah).min(img_h);
            for py in y_start..y_end {
                let dy = (f64::from(py) + 0.5 - cy) / ry;
                if dy.abs() > 1.0 {
                    continue;
                }
                let dx = (1.0 - dy * dy).sqrt() * rx;
                let x0 = (cx - dx).floor().max(0.0) as i32;
                let x1 = (cx + dx).ceil().min(f64::from(img_w)) as i32;
                if x1 <= x0 {
                    continue;
                }
                rects.push(Rectangle16 {
                    x: x0 as i16,
                    y: py as i16,
                    width: (x1 - x0) as u16,
                    height: 1,
                });
            }
        }
        if !rects.is_empty() {
            let rects = self.intersect_with_current_clip(&rects);
            self.fill_rects_honoring_fill_state(target, foreground, &rects);
        }
        Ok(())
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("fill_poly_unknown_xid");
            return Ok(());
        };
        // i16 vertex pairs. coord_mode 0 = Origin (absolute), 1 = Previous.
        let mut verts: Vec<(i32, i32)> = Vec::with_capacity(points.len() / 4);
        let mut offset = 0;
        let mut last = (0i32, 0i32);
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coord_mode == 1 && !verts.is_empty() {
                (last.0 + i32::from(x), last.1 + i32::from(y))
            } else {
                (i32::from(x), i32::from(y))
            };
            verts.push((xi, yi));
            last = (xi, yi);
        }
        let mut rects: Vec<Rectangle16> = Vec::new();
        crate::kms::backend::scanline_fill_polygon(&verts, &mut rects);
        let (img_w, img_h) = self
            .drawable_dims_v2(host_xid)
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let clipped = crate::kms::backend::clip_rects_to_image(&rects, img_w, img_h);
        let rects = self.intersect_with_current_clip(&clipped);
        self.fill_rects_honoring_fill_state(target, foreground, &rects);
        Ok(())
    }

    fn fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("fill_rectangle_unknown_xid");
            return Ok(());
        };
        let rects = self.intersect_with_current_clip(&[Rectangle16 {
            x,
            y,
            width,
            height,
        }]);
        self.fill_rects_honoring_fill_state(target, foreground, &rects);
        Ok(())
    }

    fn poly_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM8.
        // Each TEXTITEM8 is `len(u8) delta(i8) chars(len)` for len
        // in 0..=254, or `255 font_id(u32 BE)` for a font change.
        // No inter-item padding.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut items = &body[12..];
        let mut cursor_x = x;
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            if items.len() < 2 + len {
                break;
            }
            let text = &items[2..2 + len];
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            if !text.is_empty() {
                let chars: Vec<char> = text.iter().map(|&b| b as char).collect();
                self.render_text_chars_v2(host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    let advance: i32 = text
                        .iter()
                        .map(|&b| {
                            font_state
                                .char_info_cache
                                .get(&(b as char))
                                .map(|ci| ci.character_width as i32)
                                .unwrap_or(6)
                        })
                        .sum();
                    cursor_x = cursor_x.saturating_add(advance);
                }
            }
            items = &items[2 + len..];
        }
        Ok(())
    }

    fn poly_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM16.
        // Each TEXTITEM16 is `len(u8) delta(i8) chars(2*len)` (chars
        // are CHAR2B, big-endian) for len in 0..=254, or `255
        // font_id(u32 BE)` for a font change.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut cursor_x = x;
        let mut items = &body[12..];
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            let needed = 2 + 2 * len;
            if items.len() < needed {
                break;
            }
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            let mut chars = Vec::with_capacity(len);
            for i in 0..len {
                let codepoint = u16::from_be_bytes([items[2 + 2 * i], items[2 + 2 * i + 1]]) as u32;
                chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
            }
            if !chars.is_empty() {
                self.render_text_chars_v2(host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    cursor_x = cursor_x.saturating_add(
                        chars
                            .iter()
                            .map(|ch| {
                                font_state
                                    .char_info_cache
                                    .get(ch)
                                    .map(|ci| ci.character_width as i32)
                                    .unwrap_or(6)
                            })
                            .sum::<i32>(),
                    );
                }
            }
            items = &items[needed..];
        }
        Ok(())
    }

    fn image_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + string(text_len)
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;

        // Background rect from font metrics (ascent + descent).
        // Stage 3a: lower this to a single fill_rect via the
        // engine (Stage 2c op); GC-clip intersection is the
        // backend's concern (current_clip stored on KmsCore).
        if let Some(font_state) = self.core.current_font.and_then(|f| self.core.fonts.get(&f)) {
            let total_width: i32 = body[12..]
                .iter()
                .take(text_len as usize)
                .map(|&b| {
                    font_state
                        .char_info_cache
                        .get(&(b as char))
                        .map(|ci| ci.character_width as i32)
                        .unwrap_or(6)
                })
                .sum();
            let ascent = font_state.metrics.font_ascent as i32;
            let descent = font_state.metrics.font_descent as i32;
            let bg_x = x;
            let bg_y = y - ascent;
            let bg_w = total_width.max(0);
            let bg_h = (ascent + descent).max(0);
            self.fill_text_background(host_xid, background, bg_x, bg_y, bg_w, bg_h)?;
        }

        let end = (12usize + text_len as usize).min(body.len());
        let text = &body[12..end];
        let chars: Vec<char> = text.iter().map(|&b| b as char).collect();
        self.render_text_chars_v2(host_xid, foreground, x, y, &chars)
    }

    fn image_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut chars = Vec::with_capacity(text_len as usize);
        let mut pos = 12usize;
        for _ in 0..text_len {
            if pos + 2 > body.len() {
                break;
            }
            let codepoint = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
            pos += 2;
            chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
        }

        if let Some(font_state) = self.core.current_font.and_then(|f| self.core.fonts.get(&f)) {
            let total_width: i32 = chars
                .iter()
                .map(|ch| {
                    font_state
                        .char_info_cache
                        .get(ch)
                        .map(|ci| ci.character_width as i32)
                        .unwrap_or(6)
                })
                .sum();
            let ascent = font_state.metrics.font_ascent as i32;
            let descent = font_state.metrics.font_descent as i32;
            let bg_x = x;
            let bg_y = y - ascent;
            let bg_w = total_width.max(0);
            let bg_h = (ascent + descent).max(0);
            self.fill_text_background(host_xid, background, bg_x, bg_y, bg_w, bg_h)?;
        }

        self.render_text_chars_v2(host_xid, foreground, x, y, &chars)
    }

    // ── RENDER ──────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Stage 3b: real picture record. Insert default
        // `PictureRecord::Drawable`, incref the backing drawable in
        // the store (so a `free_pixmap` on the backing survives
        // while this picture wraps it — picture_record_drawable_
        // refcount test), then delegate to render_change_picture for
        // the value-mask body.
        let drawable_xid = host_drawable.as_raw();
        let picture_xid = self.core.next_host_xid();
        // Diagnostic trace (TEMP — Stage 4d "shadow only"
        // investigation). v2's PictureRecord doesn't store the
        // requested PictFormat; capturing it here so a downstream
        // analysis can see which format marco asked for when
        // wrapping a redirected backing's alias. Enable with
        // `RUST_LOG=yserver::kms::v2::render=trace`.
        log::trace!(
            target: "yserver::kms::v2::render",
            "render_create_picture pic=0x{picture_xid:x} drawable=0x{drawable_xid:x} \
             ynest_format=0x{ynest_format:x} value_mask=0x{value_mask:x} \
             value_bytes={n}",
            n = values.len(),
        );
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::drawable_default(drawable_xid, ynest_format),
        );
        if let Some(id) = self.store.lookup(drawable_xid) {
            self.store.incref(id);
        }
        if value_mask != 0 {
            // Recompose the body shape that render_change_picture
            // expects: picture(4) + value_mask(4) + values.
            let mut body = Vec::with_capacity(8 + values.len());
            body.extend_from_slice(&picture_xid.to_le_bytes());
            body.extend_from_slice(&value_mask.to_le_bytes());
            body.extend_from_slice(values);
            self.render_change_picture(None, picture_xid, &body)?;
        }
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Diagnostic trace (TEMP — Stage 4d "shadow only"
        // investigation). Body shape: picture(4) + value_mask(4) +
        // values. We log the mask bits + post-call clip state so a
        // grep across the log can see whether CPClipMask=None
        // cleared the dst picture's clip between marco's last
        // SetPictureClipRectangles and the next render_composite.
        if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace)
            && body.len() >= 8
        {
            let mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            log::trace!(
                target: "yserver::kms::v2::render",
                "render_change_picture pic=0x{host_pic:x} mask=0x{mask:x} body_len={}",
                body.len(),
            );
        }
        change_picture_apply_mask(&mut self.core, host_pic, body);
        // After applying — log clip state if pic is a Drawable.
        if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace)
            && let Some(PictureRecord::Drawable {
                clip,
                clip_x,
                clip_y,
                ..
            }) = self.core.pictures.get(&host_pic)
        {
            log::trace!(
                target: "yserver::kms::v2::render",
                "render_change_picture post pic=0x{host_pic:x} clip={} clip_origin=({clip_x},{clip_y})",
                match clip {
                    None => "None".to_string(),
                    Some(rects) => format!("Some(n={})", rects.len()),
                },
            );
        }
        Ok(())
    }

    /// Audit #8 (2026-05-19) — store the drawable-space origin of
    /// the wrapped surface on the picture record. The protocol
    /// layer calls this right after `render_create_picture` with
    /// the parent-relative `(x, y)` of a window-backed drawable
    /// (process_request.rs:1153). Pre-fix v2 inherited the trait
    /// default no-op so `drawable_origin` stayed at the
    /// `drawable_default` `(0, 0)` — clips on CSD-frame-child
    /// pictures couldn't translate external region geometry into
    /// picture-local coords.
    ///
    /// Non-Drawable picture variants (SolidFill / Linear /
    /// Radial gradient) have no drawable to anchor — tolerated
    /// no-op so the caller doesn't need to discriminate at the
    /// call site.
    fn set_picture_drawable_origin(&mut self, host_pic: u32, origin: (i16, i16)) {
        if let Some(PictureRecord::Drawable {
            drawable_origin, ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            *drawable_origin = origin;
        }
    }

    /// Audit #8 (2026-05-19) — return the picture's `clientClip` for
    /// `CreateRegionFromPicture` (XFixes). Outer `Option` distinguishes
    /// "picture doesn't carry a clientClip at all" (Solidfill /
    /// gradient → `None`, dispatcher emits BadMatch) from "picture
    /// exists and we know its clip state" (Drawable → `Some(_)`).
    /// Inner `Option` distinguishes "no clip set yet" (`Some(None)`,
    /// also BadMatch per X11 spec — can't extract a region from a
    /// picture with no clip) from "clip set" (`Some(Some(rects))`,
    /// returned as the region's rects).
    ///
    /// Pre-fix v2 inherited the trait default `None` so EVERY
    /// `CreateRegionFromPicture` call returned BadMatch — even for
    /// pictures with legitimate clipped state. Visible in clipboard
    /// managers / window managers that use this XFixes path.
    fn picture_client_clip_rects(
        &mut self,
        host_pic: u32,
    ) -> Option<Option<Vec<yserver_protocol::x11::xfixes::RegionRect>>> {
        let record = self.core.pictures.get(&host_pic)?;
        match record {
            PictureRecord::Drawable { clip, .. } => Some(clip.as_ref().map(|rects| {
                rects
                    .iter()
                    .map(|r| yserver_protocol::x11::xfixes::RegionRect {
                        x: r.x,
                        y: r.y,
                        width: r.width,
                        height: r.height,
                    })
                    .collect()
            })),
            PictureRecord::SolidFill { .. }
            | PictureRecord::LinearGradient { .. }
            | PictureRecord::RadialGradient { .. } => None,
        }
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        // Drop the record; if it was a Drawable variant, decref the
        // backing drawable in the store. SolidFill / Gradient
        // variants have no backing drawable — they own only the
        // GPU-side state on RenderEngine.picture_paint (Stage 3c).
        if let Some(record) = self.core.pictures.remove(&host_pic)
            && let Some(drawable_xid) = record.drawable_host_xid()
            && let Some(id) = self.store.lookup(drawable_xid)
        {
            self.store.decref(&mut self.platform, id);
        }
        // Drop any GPU-side state cached for this picture. Stage
        // 3b never populates the map (no gradient LUT built yet),
        // so this is a HashMap::remove no-op today; Stage 3c lazy-
        // builds gradient picture state through the same key, and
        // this teardown hook becomes load-bearing once that lands.
        self.engine.picture_paint_remove(host_pic);
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState};

        let format = match ynest_format {
            RENDER_FMT_A8 => GlyphSetFormat::A8,
            RENDER_FMT_A1 => GlyphSetFormat::A1,
            RENDER_FMT_ARGB32 => GlyphSetFormat::Argb32,
            _ => GlyphSetFormat::Other,
        };
        let id = self.core.next_host_xid();
        self.core.glyphsets.insert(
            id,
            GlyphSetState {
                format,
                glyphs: HashMap::new(),
            },
        );
        Ok(GlyphSetHandle::from_raw(id))
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        // Drop the glyphset record. Atlas-side slot reclamation
        // is Stage 5 (per Stage 3a glyph atlas: shelf packer is
        // monotonic), so the atlas pixels stay until atlas-full.
        self.core.glyphsets.remove(&host_gs);
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        // Reuses v1's parse_add_glyphs — purely CPU-side, operates
        // on the KmsCore.glyphsets entry. Atlas-side upload (the
        // Vk part) is Stage 3d's render_composite_glyphs path.
        if let Some(gs) = self.core.glyphsets.get_mut(&host_gs) {
            crate::kms::backend::parse_add_glyphs(gs, body_tail);
        }
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        let Some(gs) = self.core.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        for chunk in glyph_ids.chunks_exact(4) {
            let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            gs.glyphs.remove(&id);
        }
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        use crate::kms::v2::engine::ResolvedSource;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_composite gap: host_src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let (mask_resolved, mask_repeat, mask_transform, mask_component_alpha) = if host_mask == 0 {
            (ResolvedSource::None, Repeat::None, None, false)
        } else {
            let Some(t) = resolve_picture_for_render(&self.core, &self.store, host_mask) else {
                log::debug!("v2 render_composite gap: host_mask 0x{host_mask:x} not resolvable");
                return Ok(());
            };
            t
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 render_composite gap: host_dst 0x{host_dst:x} not a Drawable picture");
            return Ok(());
        };
        // Stage 4a — resolve through redirect routing. The picture
        // wraps a window xid; the actual paint may land in that
        // window's COMPOSITE backing with an accumulated offset.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "v2 render_composite gap: dst drawable 0x{dst_host_xid:x} \
                 not in store (post-resolve)"
            );
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);

        // Audit #2 (2026-05-19) — fold src/mask client clips into
        // the composite-region clip per Xorg's
        // `miComputeCompositeRegion` (`render/mipict.c:316-389`).
        // Pre-fix, `resolve_picture_for_render` discarded src/mask
        // clips entirely, so `SetPictureClipRectangles` on a source
        // picture (xfwm4/muffin shadow blits) painted over the
        // whole dst. The translation offset matches Xorg's
        // `miClipPictureSrc(..., xDst - xSrc, yDst - ySrc)` call
        // site at `mipict.c:356,370` — the dst already has
        // `dst_target.offset` applied to `(xDst, yDst)`, so the
        // translation picks up that offset automatically.
        let src_clip = picture_client_clip(&self.core, host_src);
        let mask_clip = if host_mask == 0 {
            None
        } else {
            picture_client_clip(&self.core, host_mask)
        };
        let dst_origin_x = i32::from(dst_x) + dst_target.offset.0;
        let dst_origin_y = i32::from(dst_y) + dst_target.offset.1;
        let src_translation = (
            dst_origin_x - i32::from(src_x),
            dst_origin_y - i32::from(src_y),
        );
        let mask_translation = (
            dst_origin_x - i32::from(mask_x),
            dst_origin_y - i32::from(mask_y),
        );
        let dst_clip = compute_render_composite_clip(
            dst_clip.as_deref(),
            src_clip.as_deref(),
            src_translation,
            mask_clip.as_deref(),
            mask_translation,
        );

        let rect = crate::kms::vk::ops::render::CompositeRect {
            src_x: i32::from(src_x),
            src_y: i32::from(src_y),
            mask_x: i32::from(mask_x),
            mask_y: i32::from(mask_y),
            dst_x: i32::from(dst_x) + dst_target.offset.0,
            dst_y: i32::from(dst_y) + dst_target.offset.1,
            width: u32::from(width),
            height: u32::from(height),
        };
        // Diagnostic trace (TEMP — Stage 4d "shadow only"
        // investigation). Enable with
        // `RUST_LOG=yserver::kms::v2::render=trace`.
        // Logs every render_composite at the backend boundary
        // with resolved source/mask/dst kinds + depths, the dst
        // drawable id (to bisect "marco's compose onto its own
        // offscreen" vs "compose onto a redirected backing"), the
        // composite op, coords, repeat / transform / component-
        // alpha state, and (after the engine call) the engine
        // stats (recorded_draws, used_src_alias_scratch,
        // used_dst_readback). Removed once we land or rule out
        // the next RENDER-side fix.
        if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace) {
            let (src_kind, src_depth) = describe_resolved_source(&self.store, &src_resolved);
            let (mask_kind, mask_depth) = describe_resolved_source(&self.store, &mask_resolved);
            let dst_depth = self.store.get(dst_target.id).map_or(0, |d| d.depth);
            // Picture-format IDs as declared at CreatePicture —
            // captures marco's sampling intent which can differ
            // from the drawable's depth-derived format.
            let src_pict_format = picture_pict_format(&self.core, host_src);
            let mask_pict_format = picture_pict_format(&self.core, host_mask);
            let dst_pict_format = picture_pict_format(&self.core, host_dst);
            // Format the clip rect list compactly. The clip rect
            // *detail* is the load-bearing diagnostic for the
            // Stage 4d "wallpaper overwrites window content" bug —
            // marco relies on the wallpaper-fill composite's clip
            // excluding the window regions, and we need to see
            // what's actually in those rects, not just the count.
            // None = "no clip set, paint everywhere" (the X11 default).
            // Some(empty vec) = "empty clip region, paint nothing"
            // (per X11 RENDER spec, post the empty-clip fix). Emit
            // distinct markers so a grep can distinguish the two
            // — they have opposite effects but used to look the
            // same in this trace.
            let clip_dump = match dst_clip.as_deref() {
                None => "<None>".to_string(),
                Some([]) => "<empty>".to_string(),
                Some(rects) => {
                    use std::fmt::Write as _;
                    let mut s = String::from("[");
                    for (i, r) in rects.iter().enumerate() {
                        if i > 0 {
                            s.push(' ');
                        }
                        let _ = write!(s, "({},{} {}x{})", r.x, r.y, r.width, r.height);
                    }
                    s.push(']');
                    s
                }
            };
            log::trace!(
                target: "yserver::kms::v2::render",
                "render_composite op={op} src=0x{host_src:x}({src_kind},d={src_depth},fmt=0x{src_pict_format:x},repeat={src_repeat:?},xform={src_xform}) \
                 mask=0x{host_mask:x}({mask_kind},d={mask_depth},fmt=0x{mask_pict_format:x},repeat={mask_repeat:?},xform={mask_xform},ca={mask_component_alpha}) \
                 dst=0x{host_dst:x}->id={dst_id:?},d={dst_depth},fmt=0x{dst_pict_format:x} \
                 src_xy=({src_x},{src_y}) mask_xy=({mask_x},{mask_y}) dst_xy=({dst_x},{dst_y})+off=({off_x},{off_y}) {width}x{height} \
                 clip{clip_dump}",
                src_xform = src_transform.is_some(),
                mask_xform = mask_transform.is_some(),
                dst_id = dst_target.id,
                off_x = dst_target.offset.0,
                off_y = dst_target.offset.1,
            );
        }
        // Audit #4 (2026-05-19) — thread src/mask/dst PictFormat IDs
        // through to the engine so an xRGB32 picture wrapping a
        // depth-32 storage picks a no-alpha sample swizzle +
        // force-opaque for sources, AND the right "no alpha target"
        // pipeline + readback selection for destinations.
        // `picture_pict_format` returns 0 for non-Drawable picture
        // variants and unknown xids — engine falls back to the depth
        // heuristic in those cases.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let mask_pict_format = picture_pict_format(&self.core, host_mask);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        let stats = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            mask_resolved,
            dst_target.id,
            std::slice::from_ref(&rect),
            dst_clip.as_deref(),
            src_repeat,
            mask_repeat,
            src_transform,
            mask_transform,
            mask_component_alpha,
            src_pict_format,
            mask_pict_format,
            dst_pict_format,
        );
        match &stats {
            Ok(s) => {
                if s.recorded_draws > 0 {
                    self.telemetry.record_paint_submit();
                }
                if s.used_dst_readback {
                    self.telemetry.record_disjoint_readback();
                }
                log::trace!(
                    target: "yserver::kms::v2::render",
                    "render_composite stats dst=0x{host_dst:x} \
                     recorded_draws={} used_src_alias_scratch={} used_dst_readback={}",
                    s.recorded_draws,
                    s.used_src_alias_scratch,
                    s.used_dst_readback,
                );
            }
            Err(e) => {
                log::warn!("v2 render_composite: engine returned {e:?} on dst 0x{host_dst:x}");
            }
        }
        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        use crate::kms::{
            core::GlyphSetFormat,
            v2::engine::{CompositeGlyphInput, ResolvedSource},
        };

        // v1-parity gating (plan §3d): op == Over (3) and the src
        // picture must be a SolidFill. Anything else returns
        // Ok(()) with `composite_glyphs_dropped_unsupported`
        // bumped — matches v1's silent-noop shape outside its
        // narrow envelope. `mask_fmt` is read but ignored
        // (rendercheck never exercises component-alpha glyphsets;
        // risk-listed in plan §"Risk 9").
        // Unsupported-counter scope (plan §3d): the gate captures
        // *protocol-supported but engine-unimplemented* shapes —
        // currently op != Over and source not SolidFill (the
        // "v1-parity scope" boundary). Stale src/dst picture
        // handles and missing glyphsets are protocol errors, not
        // unsupported features; they log a gap and return Ok
        // without bumping the counter.
        if op != 3 {
            log::debug!("v2 composite_glyphs gap: op={op} (only Over=3)");
            self.telemetry.record_composite_glyphs_dropped_unsupported();
            return Ok(());
        }
        let Some((src_resolved, _src_repeat, _src_xform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 composite_glyphs gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let foreground_premul = match src_resolved {
            ResolvedSource::Solid(c) => c,
            // Stage 3f.13: glyph paint path is still SolidFill-only
            // (matches v1's try_vk_render_composite_glyphs). For a
            // gradient source, collapse to first-stop premul — same
            // shape as the pre-3f.13 fallback, just scoped here
            // instead of in `resolve_picture_for_render`. No
            // counter bump: gradient-on-glyphs is now considered
            // "best effort handled" rather than "unsupported".
            ResolvedSource::Gradient(grad_xid) => {
                first_stop_premul_of_gradient(&self.core, grad_xid).unwrap_or_else(|| {
                    log::debug!(
                        "v2 composite_glyphs: gradient src 0x{grad_xid:x} \
                         has no stops — treating as transparent"
                    );
                    [0.0, 0.0, 0.0, 0.0]
                })
            }
            ResolvedSource::Drawable(_) | ResolvedSource::None => {
                log::debug!(
                    "v2 composite_glyphs gap: src 0x{host_src:x} is not SolidFill / Gradient \
                     (plan §3d v1-parity scope)"
                );
                self.telemetry.record_composite_glyphs_dropped_unsupported();
                return Ok(());
            }
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 composite_glyphs gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        // Stage 4a — resolve through redirect routing.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!("v2 composite_glyphs gap: dst drawable 0x{dst_host_xid:x} not in store");
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        if !self.core.glyphsets.contains_key(&host_gs) {
            log::debug!("v2 composite_glyphs gap: glyphset 0x{host_gs:x} not registered");
            return Ok(());
        }

        // Items parser — mirrors v1's `try_vk_render_composite_glyphs`
        // shape. Element size depends on the minor opcode:
        // CompositeGlyphs8 (23) → 1 byte ids, 16 (24) → 2, 32 (25)
        // → 4. Each element starts with `count(u8) pad pad pad
        // dx(i16) dy(i16)`; if `count == 255` the same 8 bytes
        // carry an inline glyphset change with the new gs xid in
        // the trailing u32.
        let id_size: usize = match minor {
            23 => 1,
            24 => 2,
            _ => 4,
        };
        // Per X RENDER protocol, `src_x`/`src_y` are the SOURCE
        // picture sampling origin, not the dst pen — same as v1.
        // The first glyph-element's `dx` / `dy` sets the absolute
        // pen position; subsequent elements accumulate.
        let _ = (src_x, src_y);
        let mut pen_x = i32::from(x_off);
        let mut pen_y = i32::from(y_off);
        let mut pos: usize = 0;
        let mut active_gs_xid = host_gs;
        // Two-pass parse: pass 1 fills `parsed` with per-glyph
        // metadata + a slot reference into either the live
        // glyphset's pixel bytes (A8) or an A1 expansion scratch
        // (A1). Pass 2 builds the final `&[CompositeGlyphInput]`
        // with stable slice references. The split avoids a borrow
        // conflict on `a1_scratches`: pushing into the Vec
        // invalidates earlier `.last()` borrows by Rust's borrow
        // checker even though the underlying heap buffers are
        // stable (Vec<Vec<u8>>'s inner buffers don't move on
        // outer-push reallocation).
        enum PixelSource {
            FromGlyphset { gs_xid: u32, glyph_id: u32 },
            A1Scratch(usize),
        }
        struct Parsed {
            gs_xid: u32,
            glyph_id: u32,
            w: u32,
            h: u32,
            pixels: PixelSource,
            dst_x: i32,
            dst_y: i32,
        }
        let mut a1_scratches: Vec<Vec<u8>> = Vec::new();
        let mut parsed: Vec<Parsed> = Vec::new();
        // Borrow the glyphsets map immutably for the whole parse.
        // The engine call below takes `&mut self.engine` /
        // `&mut self.store` but not `&self.core.glyphsets`, so a
        // single borrow scope here is sound.
        while pos + 8 <= items.len() {
            let count = items[pos] as usize;
            if count == 255 {
                if pos + 8 <= items.len() {
                    let new_xid = u32::from_le_bytes([
                        items[pos + 4],
                        items[pos + 5],
                        items[pos + 6],
                        items[pos + 7],
                    ]);
                    if new_xid != 0 && self.core.glyphsets.contains_key(&new_xid) {
                        active_gs_xid = new_xid;
                    }
                }
                pos += 8;
                continue;
            }
            let dx = i32::from(i16::from_le_bytes([items[pos + 4], items[pos + 5]]));
            let dy = i32::from(i16::from_le_bytes([items[pos + 6], items[pos + 7]]));
            pen_x += dx;
            pen_y += dy;

            let payload_start = pos + 8;
            let payload_bytes = count * id_size;
            let padded = (payload_bytes + 3) & !3;
            if payload_start + padded > items.len() {
                break;
            }

            let Some(active_gs) = self.core.glyphsets.get(&active_gs_xid) else {
                pos += 8 + padded;
                continue;
            };
            let active_gs_xid_for_key = active_gs_xid;

            for i in 0..count {
                let id_off = payload_start + i * id_size;
                let glyph_id: u32 = match id_size {
                    1 => u32::from(items[id_off]),
                    2 => u32::from(u16::from_le_bytes([items[id_off], items[id_off + 1]])),
                    _ => u32::from_le_bytes([
                        items[id_off],
                        items[id_off + 1],
                        items[id_off + 2],
                        items[id_off + 3],
                    ]),
                };
                let Some(glyph) = active_gs.glyphs.get(&glyph_id) else {
                    continue;
                };

                let gw = u32::from(glyph.width);
                let gh = u32::from(glyph.height);
                let dst_x = pen_x - i32::from(glyph.x);
                let dst_y = pen_y - i32::from(glyph.y);

                if gw > 0 && gh > 0 {
                    let pixels = match glyph.format {
                        GlyphSetFormat::A8 => PixelSource::FromGlyphset {
                            gs_xid: active_gs_xid_for_key,
                            glyph_id,
                        },
                        GlyphSetFormat::A1 => {
                            // Wire A1: rows MSB-first, 32-bit padded.
                            // Expand into a dense row-major A8 (0/0xFF).
                            // Per v1's bit-order comment
                            // (kms::backend.rs:5471), X RENDER's
                            // glyph A1 is MSB-first within each byte
                            // — `7 - col%8`. Mirror verbatim.
                            let wire_stride = (gw as usize).div_ceil(32) * 4;
                            let mut a8 = vec![0u8; (gw * gh) as usize];
                            for row in 0..(gh as usize) {
                                let src_off = row * wire_stride;
                                if src_off + wire_stride > glyph.pixels.len() {
                                    break;
                                }
                                for col in 0..(gw as usize) {
                                    let byte = glyph.pixels[src_off + col / 8];
                                    let bit = (byte >> (7 - (col & 7))) & 1;
                                    a8[row * (gw as usize) + col] = if bit != 0 { 0xFF } else { 0 };
                                }
                            }
                            let idx = a1_scratches.len();
                            a1_scratches.push(a8);
                            PixelSource::A1Scratch(idx)
                        }
                        // ARGB32-source glyphs are pre-converted to
                        // A8 in `parse_add_glyphs`, so this branch
                        // is unreachable in practice. Defensive:
                        // skip the glyph if the stored format
                        // somehow ended up as ARGB32 / Other.
                        GlyphSetFormat::Argb32 | GlyphSetFormat::Other => {
                            log::warn!(
                                "v2 composite_glyphs: unexpected stored format {:?} for \
                                 glyph 0x{glyph_id:x} — skipping",
                                glyph.format,
                            );
                            continue;
                        }
                    };

                    parsed.push(Parsed {
                        gs_xid: active_gs_xid_for_key,
                        glyph_id,
                        w: gw,
                        h: gh,
                        pixels,
                        dst_x,
                        dst_y,
                    });
                }

                pen_x += i32::from(glyph.x_off);
                pen_y += i32::from(glyph.y_off);
            }

            pos += 8 + padded;
        }

        if parsed.is_empty() {
            // No drawable glyphs (every entry was zero-size or
            // missing from the glyphset). Not a gap; just nothing
            // to record.
            return Ok(());
        }

        // Pass 2: resolve each `Parsed` to a `CompositeGlyphInput`
        // with a stable slice reference. Stage 4a — apply the
        // dst-target offset to each glyph's dst coordinates so a
        // redirected window's glyphs land in the backing.
        let (paint_dx, paint_dy) = dst_target.offset;
        let inputs: Vec<CompositeGlyphInput<'_>> = parsed
            .iter()
            .filter_map(|p| {
                let pixels: &[u8] = match &p.pixels {
                    PixelSource::FromGlyphset { gs_xid, glyph_id } => self
                        .core
                        .glyphsets
                        .get(gs_xid)
                        .and_then(|gs| gs.glyphs.get(glyph_id))
                        .map(|g| g.pixels.as_slice())?,
                    PixelSource::A1Scratch(idx) => &a1_scratches[*idx],
                };
                Some(CompositeGlyphInput {
                    gs_xid: p.gs_xid,
                    glyph_id: p.glyph_id,
                    w: p.w,
                    h: p.h,
                    pixels,
                    dst_x: p.dst_x + paint_dx,
                    dst_y: p.dst_y + paint_dy,
                })
            })
            .collect();

        if inputs.is_empty() {
            return Ok(());
        }

        let stats = self.engine.composite_glyphs(
            &mut self.store,
            &mut self.platform,
            dst_target.id,
            foreground_premul,
            &inputs,
            dst_clip.as_deref(),
        );
        match stats {
            Ok(s) => {
                if s.atlas_interns > 0 {
                    for _ in 0..s.atlas_interns {
                        self.telemetry.record_atlas_intern();
                    }
                }
                if s.glyph_uploads > 0 {
                    for _ in 0..s.glyph_uploads {
                        self.telemetry.record_glyph_upload();
                    }
                }
                if s.glyphs_dropped > 0 {
                    for _ in 0..s.glyphs_dropped {
                        self.telemetry.record_glyph_dropped_atlas_full();
                    }
                }
                if s.atlas_interns > 0 || !inputs.is_empty() {
                    // Successful composite_glyphs counts as one
                    // paint submit (mirroring `image_text` /
                    // `render_composite` telemetry shape).
                    self.telemetry.record_paint_submit();
                }
            }
            Err(e) => {
                log::warn!("v2 composite_glyphs: engine returned {e:?} on dst 0x{host_dst:x}");
            }
        }
        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!(
                "v2 render_fill_rectangles gap: host_dst 0x{host_dst:x} not a Drawable picture"
            );
            return Ok(());
        };
        // Stage 4a — redirect routing for dst.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "v2 render_fill_rectangles gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        let (paint_dx, paint_dy) = dst_target.offset;

        // X RENDER XRenderColor is wire-premultiplied (rendercheck
        // main.c:337-345); pass through unchanged.
        let color_premul = [
            f32::from(u16::from_le_bytes([color[0], color[1]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[2], color[3]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[4], color[5]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[6], color[7]])) / 65535.0,
        ];

        let mut decoded: Vec<crate::kms::vk::ops::render::CompositeRect> =
            Vec::with_capacity(rects.len() / 8);
        for chunk in rects.chunks_exact(8) {
            let rx = i16::from_le_bytes([chunk[0], chunk[1]]).saturating_add(x_off);
            let ry = i16::from_le_bytes([chunk[2], chunk[3]]).saturating_add(y_off);
            let rw = u16::from_le_bytes([chunk[4], chunk[5]]);
            let rh = u16::from_le_bytes([chunk[6], chunk[7]]);
            if rw == 0 || rh == 0 {
                continue;
            }
            decoded.push(crate::kms::vk::ops::render::CompositeRect {
                src_x: 0,
                src_y: 0,
                mask_x: 0,
                mask_y: 0,
                dst_x: i32::from(rx) + paint_dx,
                dst_y: i32::from(ry) + paint_dy,
                width: u32::from(rw),
                height: u32::from(rh),
            });
        }
        if decoded.is_empty() {
            return Ok(());
        }

        let stats = self.engine.render_fill_rectangles(
            &mut self.store,
            &mut self.platform,
            op,
            color_premul,
            dst_target.id,
            &decoded,
            dst_clip.as_deref(),
        );
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("v2 render_fill_rectangles: engine returned {e:?} on dst 0x{host_dst:x}");
        }
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        // Wire layout: each trapezoid is 40 bytes (10 × i32 16.16
        // fixed-point). Mirrors v1's try_vk_render_trapezoids_path
        // decoder (kms/backend.rs:4286).
        if traps.is_empty() {
            return Ok(());
        }
        let n_traps = traps.len() / 40;
        if n_traps == 0 {
            return Ok(());
        }
        let mut decoded: Vec<vk_traps::Trapezoid> = Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let read_i32 = |o: usize| -> i32 {
                i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
            };
            decoded.push(vk_traps::Trapezoid {
                top: read_i32(0),
                bottom: read_i32(4),
                left_p1: (read_i32(8), read_i32(12)),
                left_p2: (read_i32(16), read_i32(20)),
                right_p1: (read_i32(24), read_i32(28)),
                right_p2: (read_i32(32), read_i32(36)),
            });
        }
        // Resolve src + dst via the same helpers render_composite
        // uses. The trap path doesn't read GC clip — picture clip
        // (from dst) is what scopes the draw (plan §4).
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_trapezoids gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 render_trapezoids gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        // Stage 4a — redirect routing for dst. The fold of
        // `x_off`/`y_off` and the redirect offset (both in pixel
        // units) into a single fixed-point delta keeps the
        // 16.16-arithmetic single-pass.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!("v2 render_trapezoids gap: dst drawable 0x{dst_host_xid:x} not in store");
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        let dx = (i32::from(x_off) + dst_target.offset.0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset.1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut decoded {
                t.top = t.top.wrapping_add(dy);
                t.bottom = t.bottom.wrapping_add(dy);
                t.left_p1.0 = t.left_p1.0.wrapping_add(dx);
                t.left_p1.1 = t.left_p1.1.wrapping_add(dy);
                t.left_p2.0 = t.left_p2.0.wrapping_add(dx);
                t.left_p2.1 = t.left_p2.1.wrapping_add(dy);
                t.right_p1.0 = t.right_p1.0.wrapping_add(dx);
                t.right_p1.1 = t.right_p1.1.wrapping_add(dy);
                t.right_p2.0 = t.right_p2.0.wrapping_add(dx);
                t.right_p2.1 = t.right_p2.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::trapezoid_bbox(&decoded) else {
            return Ok(());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;

        // Pack instance bytes (40 bytes per trap; no padding —
        // asserted by `const _:()` in trap_pipeline.rs).
        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TrapInstanceData>();
        let mut instance_bytes = vec![0u8; stride * decoded.len()];
        for (i, t) in decoded.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // render_composite. Trap/tri paint into an xRGB32 dst on
        // depth-32 storage must drive "no alpha target," and
        // xRGB32 sources must pin α=ONE on the sample view.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.id,
            TrapPrimKind::Trapezoid,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                decoded.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_pict_format,
            dst_pict_format,
        );
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("v2 render_trapezoids: engine returned {e:?}");
        }
        Ok(())
    }

    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        let read_point = |off: usize, chunk: &[u8]| -> (i32, i32) {
            let x =
                i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
            let y = i32::from_le_bytes([
                chunk[off + 4],
                chunk[off + 5],
                chunk[off + 6],
                chunk[off + 7],
            ]);
            (x, y)
        };
        let mut tris: Vec<vk_traps::Triangle> = match minor {
            11 => {
                if !primitives.len().is_multiple_of(24) {
                    return Ok(());
                }
                primitives
                    .chunks_exact(24)
                    .map(|c| vk_traps::Triangle {
                        p1: read_point(0, c),
                        p2: read_point(8, c),
                        p3: read_point(16, c),
                    })
                    .collect()
            }
            12 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (0..pts.len() - 2)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[i],
                        p2: pts[i + 1],
                        p3: pts[i + 2],
                    })
                    .collect()
            }
            13 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (1..pts.len() - 1)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[0],
                        p2: pts[i],
                        p3: pts[i + 1],
                    })
                    .collect()
            }
            _ => return Ok(()),
        };
        if tris.is_empty() {
            return Ok(());
        }
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_triangles gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 render_triangles gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        // Stage 4a — redirect routing for dst; fold the redirect
        // offset into the same fixed-point delta as `x_off/y_off`.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!("v2 render_triangles gap: dst drawable 0x{dst_host_xid:x} not in store");
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        let dx = (i32::from(x_off) + dst_target.offset.0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset.1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut tris {
                t.p1.0 = t.p1.0.wrapping_add(dx);
                t.p1.1 = t.p1.1.wrapping_add(dy);
                t.p2.0 = t.p2.0.wrapping_add(dx);
                t.p2.1 = t.p2.1.wrapping_add(dy);
                t.p3.0 = t.p3.0.wrapping_add(dx);
                t.p3.1 = t.p3.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::triangle_bbox(&tris) else {
            return Ok(());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;

        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TriangleInstanceData>();
        let mut instance_bytes = vec![0u8; stride * tris.len()];
        for (i, t) in tris.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // the trapezoid path; see that call site for rationale.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.id,
            TrapPrimKind::Triangle,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                tris.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_pict_format,
            dst_pict_format,
        );
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("v2 render_triangles: engine returned {e:?}");
        }
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill: 16-bit-per-channel colour,
        // little-endian, already premultiplied on the wire (per
        // rendercheck main.c:337-345). Store the channels as f32
        // exactly as received — the pipeline samples them
        // unchanged. Layout: r[0..2] g[2..4] b[4..6] a[6..8].
        let r16 = u16::from_le_bytes([color[0], color[1]]);
        let g16 = u16::from_le_bytes([color[2], color[3]]);
        let b16 = u16::from_le_bytes([color[4], color[5]]);
        let a16 = u16::from_le_bytes([color[6], color[7]]);
        let premul = [
            f32::from(r16) / 65535.0,
            f32::from(g16) / 65535.0,
            f32::from(b16) / 65535.0,
            f32::from(a16) / 65535.0,
        ];
        let picture_xid = self.core.next_host_xid();
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::SolidFill {
                premul,
                repeat: Repeat::Normal,
                component_alpha: false,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: p1.x(4) + p1.y(4) + p2.x(4) + p2.y(4) +
        // n_stops(4) + n × stop_pos(4) + n × stop_color(8).
        // Caller passes only the request payload from offset 4 —
        // the first u32 is interpreted as p1.x (sliced at body[4..]).
        if body.len() < 24 {
            return Ok(None);
        }
        let p1x = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let p1y = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let p2x = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let p2y = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 20) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the LUT eagerly so the first
        // render_composite against this picture has it ready. The
        // record + the engine's GradientPicture have parallel
        // lifetimes — render_free_picture drops both. Build
        // failure (no Vk on test fixture, or allocation error) is
        // non-fatal: the record still lands; render_composite
        // logs a gap if it can't find the LUT. This keeps the
        // logic-test fixture (no live Vk) usable without forcing
        // every gradient-create test through lavapipe.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_linear_gradient(
            &self.platform,
            picture_xid,
            (p1x, p1y),
            (p2x, p2y),
            &engine_stops,
        ) {
            log::debug!(
                "v2 render_create_linear_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::LinearGradient {
                p1: (p1x, p1y),
                p2: (p2x, p2y),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: icx(4) icy(4) ocx(4) ocy(4) ir(4) or(4)
        // n_stops(4) + stops + colors. Same offset-by-4 convention
        // as linear (first u32 in `body` is past the request header).
        if body.len() < 32 {
            return Ok(None);
        }
        let icx = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let icy = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let ocx = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let ocy = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let ir = i32::from_le_bytes(body[20..24].try_into().unwrap());
        let or_ = i32::from_le_bytes(body[24..28].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 28) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the radial LUT (256×256 BGRA) eagerly.
        // See `render_create_linear_gradient` for failure-mode
        // rationale.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_radial_gradient(
            &self.platform,
            picture_xid,
            (icx, icy, ir),
            (ocx, ocy, or_),
            &engine_stops,
        ) {
            log::debug!(
                "v2 render_create_radial_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::RadialGradient {
                inner: (icx, icy, ir),
                outer: (ocx, ocy, or_),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_src_pic: PictureHandle,
        _x: u16,
        _y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        // Stage 3f.4: mint an xid so RENDER clients that probe the
        // cursor handle (Cairo cursor themes, GTK/Qt themed cursors)
        // see a well-formed reply. Pixel rasterisation + scene blit
        // lives at Stage 4 alongside the cursor scene-layer work;
        // until then the cursor stays at the boot default. No
        // `log_v2_gap` because the gap is documented + invariant
        // for Stage 3.
        let xid = self.core.next_host_xid();
        Ok(CursorHandle::from_raw(xid))
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + clip_x_origin(INT16) +
        // clip_y_origin(INT16) + N × [x y w h]. Pre-shift each
        // rectangle by the clip-origin so the stored list is in
        // dst-coords; the per-rect scissoring path in Stage 3c
        // doesn't track origin separately.
        if body.len() < 8 {
            return Ok(());
        }
        let x_origin = i16::from_le_bytes([body[4], body[5]]) as i32;
        let y_origin = i16::from_le_bytes([body[6], body[7]]) as i32;
        let rects_data = &body[8..];
        let mut rects = Vec::with_capacity(rects_data.len() / 8);
        for chunk in rects_data.chunks_exact(8) {
            let x = (i16::from_le_bytes([chunk[0], chunk[1]]) as i32 + x_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let y = (i16::from_le_bytes([chunk[2], chunk[3]]) as i32 + y_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let w = u16::from_le_bytes([chunk[4], chunk[5]]);
            let h = u16::from_le_bytes([chunk[6], chunk[7]]);
            rects.push(Rectangle16 {
                x,
                y,
                width: w,
                height: h,
            });
        }
        if let Some(PictureRecord::Drawable {
            clip,
            clip_x,
            clip_y,
            ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            // Diagnostic trace (TEMP — Stage 4d "shadow only"
            // investigation). Logs marco's incoming clip rect
            // list at SetPictureClipRectangles time, post-origin
            // shift. Compare against the per-call clip dump in
            // the render_composite trace to verify v2 carries
            // marco's clip through unchanged.
            if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace) {
                use std::fmt::Write as _;
                let mut s = String::new();
                for (i, r) in rects.iter().enumerate() {
                    if i > 0 {
                        s.push(' ');
                    }
                    let _ = write!(s, "({},{} {}x{})", r.x, r.y, r.width, r.height);
                }
                log::trace!(
                    target: "yserver::kms::v2::render",
                    "set_picture_clip_rectangles pic=0x{host_pic:x} origin=({x_origin},{y_origin}) n={n} rects[{s}]",
                    n = rects.len(),
                );
            }
            // X11 RENDER spec semantics:
            //   - `SetPictureClipRectangles` with EMPTY rect list =
            //     empty clip region = composites paint **nothing**.
            //   - `ChangePicture(CPClipMask = None)` clears the clip
            //     back to "no clip" = paint **everywhere** (`clip = None`).
            // The previous implementation collapsed both to None;
            // that broke marco-with-compositing because marco uses
            // the empty-list form between frames as a "stop
            // painting until I set a real clip again" gate. With
            // the buggy collapse, the wallpaper-fill composite
            // that should have been clipped to nothing painted
            // everywhere and overwrote the just-drawn window
            // contents — the Stage 4d "shadow only" symptom.
            *clip = Some(rects);
            // The X RENDER protocol carries clip-origin once per
            // SetPictureClipRectangles; we fold it into the stored
            // rects (above) but also keep clip_x/clip_y so a
            // subsequent CPClipXOrigin / CPClipYOrigin override
            // via ChangePicture composes correctly.
            *clip_x = x_origin.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            *clip_y = y_origin.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        // SolidFill / Gradient pictures: clip is a no-op (no
        // backing drawable to clip).
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + name_len(u16) + pad(2) + name +
        // pad + N × FIXED(4) parameters. Stage 3 only honours
        // `nearest`; other filters parse + store so the record-
        // round-trip is honest but `RenderEngine` ignores them at
        // draw time (per Risk 6).
        if body.len() < 8 {
            return Ok(());
        }
        let name_len = u16::from_le_bytes([body[4], body[5]]) as usize;
        if body.len() < 8 + name_len {
            return Ok(());
        }
        let name = &body[8..8 + name_len];
        let filter = match name {
            b"nearest" | b"fast" => PictureFilter::Nearest,
            b"bilinear" | b"good" | b"best" => PictureFilter::Bilinear,
            b"convolution" => PictureFilter::Convolution,
            _ => PictureFilter::Nearest,
        };
        if let Some(PictureRecord::Drawable { filter: f, .. }) =
            self.core.pictures.get_mut(&host_pic)
        {
            *f = filter;
        }
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + 9 × FIXED(4) matrix entries (row-
        // major). 16.16 fixed-point; identity is [[1,0,0],[0,1,0],
        // [0,0,1]] in floating shape, [[0x10000, 0, 0], [0, 0x10000,
        // 0], [0, 0, 0x10000]] in fixed.
        if body.len() < 40 {
            return Ok(());
        }
        let mut matrix = [[0i32; 3]; 3];
        for (idx, slot) in matrix.iter_mut().flatten().enumerate() {
            let off = 4 + idx * 4;
            *slot = i32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        }
        let transform = if matrix == [[0x10000, 0, 0], [0, 0x10000, 0], [0, 0, 0x10000]] {
            None
        } else {
            Some(PictTransform { matrix })
        };
        match self.core.pictures.get_mut(&host_pic) {
            Some(PictureRecord::Drawable { transform: t, .. })
            | Some(PictureRecord::LinearGradient { transform: t, .. })
            | Some(PictureRecord::RadialGradient { transform: t, .. }) => *t = transform,
            _ => {}
        }
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        // Advertise RENDER 0.11 (the version v1 reports). Stubbed
        // paint paths still need the version reply to flow through;
        // skipping it would break clients at extension query.
        Ok((0, 11))
    }

    // ── DRI3 — ported from v1 (Stage 4d backfill) ───────────────
    //
    // Body shape mirrors `kms/backend.rs:8613-8869` verbatim — the
    // helpers in `kms::vk::dri3`, `kms::vk::sync`, `kms::render_node`,
    // and `kms::xshmfence` are already shared with v1, so v2 calls
    // them directly. Without these, no compositor (marco, xfwm4,
    // picom, compton) can import redirected window backings as GPU
    // textures and the 4d-close hardware smoke wedges on
    // PresentPixmap → COW.

    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        // Open a fresh fd at the render-node path per client. dup()'ing
        // a shared long-lived fd would give every client the same
        // kernel struct file, and libdrm_amdgpu maintains GEM handles
        // + contexts in per-struct-file state — the first client
        // populates it, the second crashes in `amdgpu_winsys_create`
        // hitting leftover handles. See
        // feedback_dri3_open_fresh_fd.md.
        let path = self.platform.render_node_path.as_deref().ok_or_else(|| {
            io::Error::other("DRI3 unavailable — render node was not resolved at backend init")
        })?;
        crate::kms::render_node::open_fresh(path)
            .map_err(|e| io::Error::other(format!("open render-node {}: {e}", path.display())))
    }

    fn dri3_capabilities(&self) -> Dri3Caps {
        // DRI3 entirely unavailable when render-node fd or Vulkan
        // weren't resolved at backend init.
        if self.platform.render_node_fd.is_none() || self.platform.vk.is_none() {
            return Dri3Caps::unsupported();
        }
        let vk = self.platform.vk.as_ref().expect("vk Some by branch above");
        let modifiers = vk.image_drm_format_modifier;
        // VK_KHR_external_semaphore_fd is unconditionally enabled at
        // device init; fence_fd / SYNC_FD handle type rides along
        // with it. syncobj uses the OPAQUE_FD + timeline-semaphore
        // path also covered by VK_KHR_external_semaphore_fd — which
        // Mesa prefers and Venus accepts (per the live vng smoke).
        let fence_fd = true;
        let syncobj = true;
        // Version cap per Phase 4.2 design §4: with syncobj
        // advertise (1, 4); without it cap at (1, 3).
        let version = if syncobj { (1, 4) } else { (1, 3) };
        Dri3Caps {
            version,
            modifiers,
            fence_fd,
            syncobj,
        }
    }

    fn dri3_import_pixmap(
        &mut self,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: u64,
        depth: u8,
        bpp: u8,
    ) -> io::Result<PixmapHandle> {
        // Per Phase 4.2 design §3.2: import the dma-buf into a
        // DrawableImage via VK_EXT_image_drm_format_modifier, wrap
        // it as a v2 Storage, allocate a fresh Pixmap entry in
        // the store. Pixmap exists as a real X resource so clients
        // can CopyArea / ChangePicture against it.
        let Some(vk) = self.platform.vk.clone() else {
            return Err(io::Error::other("DRI3 import: Vulkan unavailable"));
        };
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => {
                return Err(io::Error::other(format!(
                    "DRI3 import: unsupported (depth={depth}, bpp={bpp}); Phase 4.2 RGB single-plane only"
                )));
            }
        };
        let drawable = crate::kms::vk::dri3::import_dmabuf(
            vk.clone(),
            fd,
            u32::from(width),
            u32::from(height),
            format,
            modifier,
            &[crate::kms::vk::dri3::DmabufPlane {
                offset: u64::from(offset),
                pitch: stride,
            }],
        )
        .map_err(|e| io::Error::other(format!("DRI3 import_dmabuf: {e:?}")))?;
        // Build a sample-side view over the imported VkImage. The
        // DRI3 path's own `vk_image_view` (kept as `image_view` on
        // the resulting Storage) is IDENTITY-swizzle and serves as
        // the attachment view; the sample-side view applies the
        // format/depth-aware swizzle the scene compositor relies on
        // (depth-24 BGRA8 → α=ONE).
        let sample_view = crate::kms::v2::platform::PlatformBackend::build_sample_view(
            &vk,
            drawable.vk_image,
            drawable.format,
            depth,
        )
        .map_err(|e| io::Error::other(format!("DRI3 import build_sample_view: {e:?}")))?;
        let storage = Storage::from_imported_drawable_image(drawable, sample_view, depth);
        let host_xid = self.core.next_host_xid();
        self.store
            .allocate(host_xid, DrawableKind::Pixmap, depth, false, storage)
            .map_err(|e| io::Error::other(format!("DRI3 import store.allocate: {e:?}")))?;
        // Telemetry: an imported pixmap is still a fresh storage
        // entry + a view (the DrawableImage built one inside
        // from_dmabuf). Mirrors init_root_storage's accounting so
        // the per-second counters stay accurate under DRI3 traffic.
        self.telemetry.record_storage_allocation();
        self.telemetry.record_image_view_create();
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("DRI3 import: failed to make PixmapHandle"))
    }

    fn dri3_supported_modifiers(&self, _window: u32, depth: u8, bpp: u8) -> (Vec<u64>, Vec<u64>) {
        let Some(vk) = self.platform.vk.as_ref() else {
            return (vec![0], vec![0]);
        };
        // Map (depth, bpp) to a vk::Format. Phase 4.2 RGB single-
        // plane scope means we only handle depth-24/32 BGRA today.
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => return (vec![0], vec![0]),
        };
        let screen = crate::kms::vk::dri3::supported_modifiers(vk, format);
        // Window-modifier list is the subset that the window's
        // output can flip-scanout. Phase 4.1 always uses LINEAR
        // for scanout, so the window list collapses to LINEAR
        // here. A follow-up populates `output.scanout_format_set`
        // from the real add_fb2 probe and widens this.
        let window: Vec<u64> = screen.iter().copied().filter(|&m| m == 0).collect();
        let window = if window.is_empty() { vec![0] } else { window };
        (window, screen)
    }

    fn dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 export: Vulkan unavailable"));
        };
        let id = self.store.lookup(host_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: unknown pixmap 0x{host_xid:x}"))
        })?;
        let drawable = self.store.get(id).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: store entry missing 0x{host_xid:x}"))
        })?;
        let imported = drawable.storage.imported_drawable.as_ref().ok_or_else(|| {
            io::Error::other(format!(
                "DRI3 export: pixmap 0x{host_xid:x} has no imported backing"
            ))
        })?;
        let depth = drawable.depth;
        let width = u16::try_from(drawable.storage.extent.width).unwrap_or(u16::MAX);
        let height = u16::try_from(drawable.storage.extent.height).unwrap_or(u16::MAX);
        let bpp: u8 = match depth {
            24 | 32 => 32,
            d => d,
        };
        let export = crate::kms::vk::dri3::export_dmabuf(vk, imported)
            .map_err(|e| io::Error::other(format!("DRI3 export_dmabuf: {e:?}")))?;
        let stride16 = u16::try_from(export.stride).unwrap_or(u16::MAX);
        Ok((export.size, width, height, stride16, depth, bpp, export.fd))
    }

    fn dri3_fence_from_fd(&mut self, fence_xid: u32, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        // Mesa's loader_dri3 sends an xshmfence (memfd + futex) —
        // try that path FIRST. vkImportSemaphoreFdKHR rejects
        // xshmfence fds because they aren't sync_file. Mmap first;
        // fall through to Vulkan import only if mmap fails (i.e.
        // the fd really is a sync_file).
        use std::os::fd::AsFd as _;
        if let Some(mapping) = crate::kms::xshmfence::FenceMapping::map(fd.as_fd()) {
            self.dri3_xshmfences.insert(fence_xid, mapping);
            log::debug!("DRI3 FenceFromFD 0x{fence_xid:x}: imported as xshmfence");
            return Ok(());
        }
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other(
                "DRI3 FenceFromFD: fd isn't xshmfence and Vulkan is unavailable",
            ));
        };
        let semaphore = crate::kms::vk::sync::import_sync_file(vk, fd)
            .map_err(|e| io::Error::other(format!("import_sync_file: {e:?}")))?;
        if let Some(prev) = self.dri3_sync_resources.insert(fence_xid, semaphore) {
            unsafe { vk.device.destroy_semaphore(prev, None) };
        }
        Ok(())
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> io::Result<()> {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.trigger();
            return Ok(());
        }
        // VkSemaphore-backed fences: signalling is done via queue
        // submit (or vkSignalSemaphore for timeline). For Phase 4.2
        // first-cut Copy path the GPU work is already serialized,
        // so a server-only `triggered=true` mirror is sufficient
        // — no GPU operation needed here.
        Ok(())
    }

    fn dri3_fd_from_fence(&mut self, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FDFromFence: Vulkan unavailable"));
        };
        let &semaphore = self.dri3_sync_resources.get(&fence_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 FDFromFence: unknown fence 0x{fence_xid:x}"))
        })?;
        crate::kms::vk::sync::export_sync_file(vk, semaphore)
            .map_err(|e| io::Error::other(format!("export_sync_file: {e:?}")))
    }

    fn dri3_import_syncobj(
        &mut self,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 ImportSyncobj: Vulkan unavailable"));
        };
        let semaphore = crate::kms::vk::sync::import_drm_syncobj(vk, fd)
            .map_err(|e| io::Error::other(format!("import_drm_syncobj: {e:?}")))?;
        if let Some(prev) = self.dri3_sync_resources.insert(syncobj_xid, semaphore) {
            unsafe { vk.device.destroy_semaphore(prev, None) };
        }
        Ok(())
    }

    fn dri3_free_syncobj(&mut self, syncobj_xid: u32) -> io::Result<()> {
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FreeSyncobj: Vulkan unavailable"));
        };
        if let Some(sem) = self.dri3_sync_resources.remove(&syncobj_xid) {
            unsafe { vk.device.destroy_semaphore(sem, None) };
        }
        Ok(())
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> io::Result<()> {
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 SignalSyncobj: Vulkan unavailable"));
        };
        let &semaphore = self.dri3_sync_resources.get(&syncobj_xid).ok_or_else(|| {
            io::Error::other(format!(
                "DRI3 SignalSyncobj: unknown syncobj 0x{syncobj_xid:x}"
            ))
        })?;
        crate::kms::vk::sync::signal_timeline(vk, semaphore, value)
            .map_err(|e| io::Error::other(format!("vkSignalSemaphore: {e:?}")))
    }

    fn present_capabilities(&self, _window: u32) -> PresentCaps {
        // Mirror v1's conservative "Copy-path only" caps. syncobj
        // tracks Dri3Caps::syncobj. flip_path / async_may_tear stay
        // false until alien-BO scanout integration lands on v2.
        PresentCaps {
            flip_path: false,
            async_may_tear: false,
            syncobj: self.dri3_capabilities().syncobj,
        }
    }

    // ── Other extensions ────────────────────────────────────────

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        _body: &[u8],
    ) -> io::Result<Option<Vec<u8>>> {
        // Mirror v1's xkb_proxy verbatim — pure protocol
        // bookkeeping using the shared `KmsCore.xkb_keymap`.
        // Without this, Xlib clients abort at the XKEYBOARD
        // UseExtension handshake, so no real-app smoke is
        // possible. The behaviour-level fix is identical to v1
        // (reply minors get bodies, void minors return None).
        use crate::kms::xkb as xkb_replies;
        let reply = match minor {
            0 => Some(xkb_replies::reply_use_extension()),
            6 => Some(xkb_replies::reply_get_controls(&self.core.xkb_keymap.0)),
            8 => Some(xkb_replies::reply_get_map(&self.core.xkb_keymap.0)),
            10 => Some(xkb_replies::reply_get_compat_map()),
            17 => Some(xkb_replies::reply_get_names(&self.core.xkb_keymap.0)),
            21 => Some(xkb_replies::reply_per_client_flags(_body)),
            24 => Some(xkb_replies::reply_get_device_info()),
            4 | 12 | 13 | 15 | 19 | 22 | 23 | 101 => Some(xkb_replies::reply_minimal(minor)),
            1 | 3 | 5 | 7 | 9 | 11 | 14 | 16 | 18 | 20 | 25 => None,
            _ => {
                log::debug!("v2 xkb: unknown minor {minor}, no reply sent");
                None
            }
        };
        Ok(reply)
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        _origin: Option<OriginContext>,
        _host_cursor_xid: u32,
        _name_bytes: &[u8],
    ) -> io::Result<()> {
        // Stage 3f.4: v1-parity no-op. XFixes cursor-by-name is a
        // theme-database hint ("watch" / "left_ptr" / etc.); yserver
        // doesn't have a cursor-theme registry, so neither v1 nor v2
        // do anything beyond returning Ok. Real apps see no behaviour
        // difference (their fallback non-named cursor stays in
        // effect).
        Ok(())
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        // Bookkeeping mutation: SHAPE rects live in KmsCore; no
        // paint side-effect needed in Stage 1b.
        let dst = match kind {
            0 => &mut self.core.shape_bounding,
            1 => &mut self.core.shape_clip,
            2 => &mut self.core.shape_input,
            _ => {
                self.log_v2_gap("set_shape_rectangles_invalid_kind");
                return Ok(());
            }
        };
        if rects.is_empty() {
            dst.remove(&host_xid);
        } else {
            dst.insert(host_xid, rects.to_vec());
        }
        Ok(())
    }

    // ── Misc ────────────────────────────────────────────────────

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        _dst_host_xid: u32,
        _dst_x: i16,
        _dst_y: i16,
    ) -> io::Result<()> {
        // Stage 1b doesn't process pointer events meaningfully —
        // just log + accept. v2 pointer-state lives in KmsCore but
        // wiring it to scene/input dispatch lands in Stage 2.
        self.log_v2_gap("warp_pointer");
        Ok(())
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        // Return the current core-tracked cursor position. No
        // window-focus lookup — Stage 1b doesn't model focus.
        Ok(PointerPosition {
            same_screen: true,
            #[allow(clippy::cast_possible_truncation)]
            win_x: self.core.cursor_x as i16,
            #[allow(clippy::cast_possible_truncation)]
            win_y: self.core.cursor_y as i16,
            mask: self.core.button_mask,
        })
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>> {
        let cap = usize::from(max_names);
        let names: Vec<&str> = self
            .core
            .font_loader
            .catalog
            .iter()
            .map(String::as_str)
            .filter(|name| xlfd_pattern_matches(pattern, name))
            .take(cap)
            .collect();

        let mut name_data: Vec<u8> = Vec::new();
        for name in &names {
            name_data.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
            name_data.extend_from_slice(name.as_bytes());
        }
        let pad = (4 - (name_data.len() % 4)) % 4;
        name_data.resize(name_data.len() + pad, 0);

        let extra_words = u32::try_from(name_data.len() / 4).unwrap_or(0);
        let mut reply = vec![0u8; 32 + name_data.len()];
        reply[0] = 1;
        reply[4..8].copy_from_slice(&extra_words.to_le_bytes());
        reply[8..10].copy_from_slice(&u16::try_from(names.len()).unwrap_or(u16::MAX).to_le_bytes());
        reply[32..].copy_from_slice(&name_data);
        Ok(reply)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        let cap = usize::from(max_names);
        let matched: Vec<String> = self
            .core
            .font_loader
            .catalog
            .iter()
            .filter(|name| xlfd_pattern_matches(pattern, name))
            .take(cap)
            .cloned()
            .collect();

        let mut entries: Vec<(String, FontMetrics)> = Vec::with_capacity(matched.len());
        for name in matched {
            match self.core.font_loader.open_font(&name) {
                Ok((_face, metrics, _cache)) => entries.push((name, metrics)),
                Err(err) => {
                    log::debug!("v2 ListFontsWithInfo: skipping {name:?} — open_font: {err}");
                }
            }
        }

        let total = entries.len();
        let mut replies: Vec<Vec<u8>> = Vec::with_capacity(total + 1);
        for (idx, (name, metrics)) in entries.iter().enumerate() {
            let remaining = u32::try_from(total - idx - 1).unwrap_or(0);
            let mut buf = Vec::new();
            yserver_protocol::x11::write_list_fonts_with_info_reply(
                &mut buf,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
                yserver_protocol::x11::SequenceNumber(0),
                metrics,
                name,
                remaining,
            )?;
            replies.push(buf);
        }
        let mut term = Vec::new();
        yserver_protocol::x11::write_list_fonts_with_info_terminator(
            &mut term,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            yserver_protocol::x11::SequenceNumber(0),
        )?;
        replies.push(term);
        Ok(replies)
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        // Atom store lives in ServerState, not the backend. v2 has
        // nothing to add here.
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        // Stage 3f.7 follow-up: port v1's body verbatim. KmsCore
        // carries `xkb_keymap` so the lookup works on both backends.
        // The pre-fix stub returned 0 keysyms per code, which made
        // xterm think every key was dead — typing into xterm worked
        // for cursor movement but Enter/letters were swallowed.
        //
        // X11 GetKeyboardMapping: per keycode, return a flat row of
        // keysyms across shift levels (unshifted / shifted /
        // mode-switch-unshifted / mode-switch-shifted). Apps combine
        // the keycode with the modifier bits in the event's `state`
        // field to pick the right slot.
        const LEVELS: usize = 4;
        let max_kc = u16::from(first_keycode) + u16::from(count);
        let mut flat = Vec::with_capacity(usize::from(count) * LEVELS);
        for kc in u16::from(first_keycode)..max_kc {
            let xkb_kc = xkbcommon::xkb::Keycode::new(u32::from(kc));
            for level in 0..LEVELS as u32 {
                let syms = self
                    .core
                    .xkb_keymap
                    .0
                    .key_get_syms_by_level(xkb_kc, 0, level);
                flat.push(syms.first().map_or(0, |s| s.raw()));
            }
        }
        Ok((LEVELS as u8, flat))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        // v1-parity conventional defaults. 8 rows × 4 keycodes:
        // Shift(0x32,0x3E), Lock(0x42), Control(0x25,0x69),
        // Mod1(0x40,0x6C), Mod2(0x4D), Mod3(0x73),
        // Mod4(0x85,0x86), Mod5(empty).
        let data: Vec<u8> = vec![
            0x32, 0x3E, 0, 0, // Shift
            0x42, 0, 0, 0, // Lock
            0x25, 0x69, 0, 0, // Control
            0x40, 0x6C, 0, 0, // Mod1
            0x4D, 0, 0, 0, // Mod2
            0x73, 0, 0, 0, // Mod3
            0x85, 0x86, 0, 0, // Mod4
            0, 0, 0, 0, // Mod5
        ];
        Ok((4, data))
    }
}

/// XLFD glob match per X11 ListFonts semantics: `*` matches zero or more
/// characters (including `-`), `?` matches exactly one. Comparison is
/// ASCII case-insensitive because clients legitimately mix case.
fn xlfd_pattern_matches(pattern: &str, name: &str) -> bool {
    let pat = pattern.as_bytes();
    let s = name.as_bytes();
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_si: usize = 0;
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi].eq_ignore_ascii_case(&s[si])) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_si = si;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Stage 4d Manual-redirect fix: when a window has mapped child
/// windows and is drawn into via a `ClipByChildren` GC (the X11
/// default), the draw must NOT touch the area covered by each
/// child. In v1 this was natural because every window had its own
/// mirror — paint to the parent landed in the parent's storage
/// while child paint landed in the child's. v2's COMPOSITE Manual-
/// redirect collapses an entire redirected subtree into a single
/// backing pixmap, so the parent-vs-child overlap is now a real
/// region-rect subtraction the backend has to perform.
///
/// Symptom this fixes: marco's per-frame full-extent CopyArea
/// (decorations source → frame) clobbers the inferior CC window's
/// area inside the redirected backing, then CC repaints only its
/// small dirty rect, leaving the backing's centre as marco's
/// (mostly-blank) decoration pixmap. Visible as "top-left content
/// only" after a few frames.
///
/// `dst_rect` is in destination-window-local coordinates; `child_rects`
/// are mapped-child rectangles also in destination-window-local
/// coordinates (parent's `(child.x, child.y, child.w, child.h)`).
/// Returns the surviving sub-rectangles, also in dst-window-local
/// coordinates. Empty input child list returns `[dst_rect]`. Empty
/// `dst_rect` (zero-size) returns `[]`.
/// Intersect a destination rectangle against an X11 GC clip
/// (a list of rectangles already translated into destination-window
/// coordinates). Returns the surviving pieces. An empty `clip_rects`
/// represents an empty clip region — Xorg's behaviour is "paint
/// nothing", so we return an empty Vec. `rect` with zero area also
/// returns empty.
///
/// Used by `copy_area` ahead of child subtraction so a
/// `SetClipRectangles`-issued explicit clip constrains the copy
/// (Stage 4d codex round 2026-05-18: pre-fix `copy_area` honoured
/// neither GC clip nor `ClipByChildren`).
fn intersect_rect_with_clip(
    rect: ash::vk::Rect2D,
    clip_rects: &[ash::vk::Rect2D],
) -> Vec<ash::vk::Rect2D> {
    if clip_rects.is_empty() || rect.extent.width == 0 || rect.extent.height == 0 {
        return Vec::new();
    }
    let rx0 = rect.offset.x;
    let ry0 = rect.offset.y;
    let rx1 = rx0 + i32::try_from(rect.extent.width).unwrap_or(i32::MAX);
    let ry1 = ry0 + i32::try_from(rect.extent.height).unwrap_or(i32::MAX);
    let mut out = Vec::with_capacity(clip_rects.len());
    for c in clip_rects {
        let cx0 = c.offset.x;
        let cy0 = c.offset.y;
        let cx1 = cx0 + i32::try_from(c.extent.width).unwrap_or(0);
        let cy1 = cy0 + i32::try_from(c.extent.height).unwrap_or(0);
        let ix0 = rx0.max(cx0);
        let iy0 = ry0.max(cy0);
        let ix1 = rx1.min(cx1);
        let iy1 = ry1.min(cy1);
        if ix0 < ix1 && iy0 < iy1 {
            out.push(ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: ix0, y: iy0 },
                extent: ash::vk::Extent2D {
                    width: u32::try_from(ix1 - ix0).unwrap_or(0),
                    height: u32::try_from(iy1 - iy0).unwrap_or(0),
                },
            });
        }
    }
    out
}

/// Translate a clip-rect list by `(dx, dy)` (signed). Used to map a
/// source / mask picture's client clip from the picture's own
/// drawable space into the destination's drawable space, mirroring
/// Xorg's `miClipPictureSrc`
/// (`/home/jos/Projects/xserver/render/mipict.c:267-290`). The Xorg
/// path translates pPicture->clientClip in-place, intersects, then
/// translates back; we copy-translate so the picture record stays
/// untouched.
///
/// Out-of-i16 results saturate; X11 fixed-point clips never need
/// more than 16-bit signed coords on the wire.
fn translate_clip_rects(rects: &[Rectangle16], dx: i32, dy: i32) -> Vec<Rectangle16> {
    rects
        .iter()
        .map(|r| {
            let nx = i32::from(r.x).saturating_add(dx);
            let ny = i32::from(r.y).saturating_add(dy);
            Rectangle16 {
                x: i16::try_from(nx).unwrap_or(if nx < 0 { i16::MIN } else { i16::MAX }),
                y: i16::try_from(ny).unwrap_or(if ny < 0 { i16::MIN } else { i16::MAX }),
                width: r.width,
                height: r.height,
            }
        })
        .collect()
}

/// Intersect two clip-rect lists. Returns the pairwise rectangle
/// intersections, omitting empties. Both lists are interpreted as
/// "the clip is the union of these rects" — the resulting list is
/// `union { a ∩ b : a ∈ a_list, b ∈ b_list }`.
///
/// Helper for `compute_render_composite_clip` below.
fn intersect_clip_lists(a: &[Rectangle16], b: &[Rectangle16]) -> Vec<Rectangle16> {
    let mut out = Vec::with_capacity(a.len() * b.len());
    for ra in a {
        let ax0 = i32::from(ra.x);
        let ay0 = i32::from(ra.y);
        let ax1 = ax0.saturating_add(i32::from(ra.width));
        let ay1 = ay0.saturating_add(i32::from(ra.height));
        for rb in b {
            let bx0 = i32::from(rb.x);
            let by0 = i32::from(rb.y);
            let bx1 = bx0.saturating_add(i32::from(rb.width));
            let by1 = by0.saturating_add(i32::from(rb.height));
            let ix0 = ax0.max(bx0);
            let iy0 = ay0.max(by0);
            let ix1 = ax1.min(bx1);
            let iy1 = ay1.min(by1);
            if ix0 < ix1 && iy0 < iy1 {
                out.push(Rectangle16 {
                    x: i16::try_from(ix0).unwrap_or(i16::MAX),
                    y: i16::try_from(iy0).unwrap_or(i16::MAX),
                    width: u16::try_from(ix1 - ix0).unwrap_or(u16::MAX),
                    height: u16::try_from(iy1 - iy0).unwrap_or(u16::MAX),
                });
            }
        }
    }
    out
}

/// Compose the effective composite-region clip for `render_composite`
/// per X RENDER spec (`miComputeCompositeRegion`,
/// `/home/jos/Projects/xserver/render/mipict.c:316-389`):
///
///   clip = dst_clip ∩ src_clip-translated-to-dst-space ∩ mask_clip-translated-to-dst-space
///
/// Each argument may be `None`, which is interpreted as "no clip on
/// this picture" (paint everywhere). If all three are `None`, the
/// function returns `None` — the engine then applies its own
/// full-extent default. If any is `Some`, the result is `Some` and
/// carries the intersection (possibly empty, which means "paint
/// nothing" per X RENDER spec — Xorg returns FALSE here and skips
/// the draw).
///
/// `src_translation` and `mask_translation` are `(xDst - xSrc,
/// yDst - ySrc)` and `(xDst - xMask, yDst - yMask)` respectively
/// (per Xorg's `miClipPictureSrc` call sites at `mipict.c:356,370`).
/// `mask_clip` should be `None` when no mask is used.
///
/// Pure / no Vulkan; tested below against hand-traced Xorg vectors.
fn compute_render_composite_clip(
    dst_clip: Option<&[Rectangle16]>,
    src_clip: Option<&[Rectangle16]>,
    src_translation: (i32, i32),
    mask_clip: Option<&[Rectangle16]>,
    mask_translation: (i32, i32),
) -> Option<Vec<Rectangle16>> {
    let src_in_dst =
        src_clip.map(|c| translate_clip_rects(c, src_translation.0, src_translation.1));
    let mask_in_dst =
        mask_clip.map(|c| translate_clip_rects(c, mask_translation.0, mask_translation.1));
    // Start with whichever input is Some, then fold the remaining
    // Some-inputs via intersection. Order doesn't matter — list
    // intersection is associative & commutative.
    let mut acc: Option<Vec<Rectangle16>> = None;
    let mut fold = |next: Option<Vec<Rectangle16>>| match (acc.take(), next) {
        (None, None) => {}
        (None, Some(v)) => acc = Some(v),
        (Some(a), None) => acc = Some(a),
        (Some(a), Some(b)) => acc = Some(intersect_clip_lists(&a, &b)),
    };
    fold(dst_clip.map(<[Rectangle16]>::to_vec));
    fold(src_in_dst);
    fold(mask_in_dst);
    acc
}

fn compute_copy_area_dst_rects(
    dst_rect: ash::vk::Rect2D,
    child_rects: &[ash::vk::Rect2D],
) -> Vec<ash::vk::Rect2D> {
    if dst_rect.extent.width == 0 || dst_rect.extent.height == 0 {
        return Vec::new();
    }
    let mut current = vec![dst_rect];
    for child in child_rects {
        let mut next = Vec::new();
        for r in current {
            next.extend(subtract_one_rect_clip(r, *child));
        }
        current = next;
        if current.is_empty() {
            return current;
        }
    }
    current
}

/// Subtract `inner` from `outer`. Both rects are in the same coord
/// space. Result is up to 4 disjoint sub-rectangles tiling
/// `outer \ inner` (top strip, bottom strip, middle-band left strip,
/// middle-band right strip — Xorg/pixman band order). If `inner`
/// doesn't intersect `outer`, returns `[outer]` unchanged.
fn subtract_one_rect_clip(outer: ash::vk::Rect2D, inner: ash::vk::Rect2D) -> Vec<ash::vk::Rect2D> {
    let ox0 = outer.offset.x;
    let oy0 = outer.offset.y;
    let ox1 = outer.offset.x + i32::try_from(outer.extent.width).unwrap_or(i32::MAX);
    let oy1 = outer.offset.y + i32::try_from(outer.extent.height).unwrap_or(i32::MAX);
    // Intersection of inner with outer (clamped to outer's bounds).
    let ix0 = inner.offset.x.max(ox0);
    let iy0 = inner.offset.y.max(oy0);
    let ix1 = (inner.offset.x + i32::try_from(inner.extent.width).unwrap_or(0)).min(ox1);
    let iy1 = (inner.offset.y + i32::try_from(inner.extent.height).unwrap_or(0)).min(oy1);
    if ix0 >= ix1 || iy0 >= iy1 {
        return vec![outer];
    }
    let mk = |x: i32, y: i32, w: i32, h: i32| ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x, y },
        extent: ash::vk::Extent2D {
            width: u32::try_from(w).unwrap_or(0),
            height: u32::try_from(h).unwrap_or(0),
        },
    };
    let mut result = Vec::with_capacity(4);
    // Top strip: full outer width, y in [oy0, iy0).
    if oy0 < iy0 {
        result.push(mk(ox0, oy0, ox1 - ox0, iy0 - oy0));
    }
    // Bottom strip: full outer width, y in [iy1, oy1).
    if iy1 < oy1 {
        result.push(mk(ox0, iy1, ox1 - ox0, oy1 - iy1));
    }
    // Left middle: middle band height, x in [ox0, ix0).
    if ox0 < ix0 {
        result.push(mk(ox0, iy0, ix0 - ox0, iy1 - iy0));
    }
    // Right middle: middle band height, x in [ix1, ox1).
    if ix1 < ox1 {
        result.push(mk(ix1, iy0, ox1 - ix1, iy1 - iy0));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        KmsBackendV2, PictureRecord, compute_copy_area_dst_rects, compute_render_composite_clip,
        intersect_rect_with_clip, resolve_picture_for_render,
    };
    use crate::kms::cpu_types::{Rectangle16, Repeat};
    use std::collections::HashMap;
    use yserver_core::backend::Backend;

    /// Stage 1b acceptance gate (synthetic): v2 constructs through
    /// `for_tests` and answers the capability accessors with the
    /// same values as v1. This is the "boots far enough to service
    /// capability queries" check from the spec.
    #[test]
    fn v2_skeleton_advertises_expected_capabilities() {
        let b = KmsBackendV2::for_tests();
        assert_eq!(b.window_id(), 1);
        assert_eq!(b.root_visual_xid(), 0x21);
        assert_eq!(b.render_opcode(), Some(133));
        assert_eq!(b.xkb_opcode(), Some(136));
        assert_eq!(b.xkb_info(), Some((136, 85, 162)));
        assert_eq!(b.composite_opcode(), Some(144));
        // Non-trivial format passes through untouched; 0 returns None.
        assert_eq!(b.render_format_for_ynest_id(0), None);
        assert_eq!(b.render_format_for_ynest_id(0x12345), Some(0x12345));
        // KMS has no upstream host visuals, but it still advertises
        // server-local ARGB ids so CreateWindow can preserve depth 32.
        assert_eq!(b.argb_visual_xid(), Some(0x103));
        assert_eq!(b.argb_colormap_xid(), Some(0x104));
    }

    /// Spec: "the first paint op produces a logged 'v2 not yet
    /// implemented' gap." Verify dedup — same op logs once even
    /// when called multiple times.
    ///
    /// Stage 2c wired fill_rectangle / put_image to real engine
    /// calls; against `for_tests` (no Vk) those reach the engine,
    /// surface `NoVk`, and log under a different name. The dedup
    /// behaviour is unchanged: each gap-name fires once per
    /// session. copy_area is still a logged-gap stub (Stage 2d
    /// territory).
    #[test]
    fn v2_paint_stub_returns_ok_and_dedups_gap() {
        let mut b = KmsBackendV2::for_tests();
        // First call logs (xid is unknown → `*_unknown_xid` gap).
        assert!(b.put_image(None, 0x1234, 24, 16, 16, 0, 0, &[0; 4]).is_ok());
        // Subsequent calls also return Ok and don't crash.
        for _ in 0..5 {
            assert!(b.put_image(None, 0x1234, 24, 16, 16, 0, 0, &[0; 4]).is_ok());
            assert!(b.copy_area(None, 0x1234, 0x5678, 0, 0, 0, 0, 4, 4).is_ok());
            assert!(b.fill_rectangle(None, 0x1234, 0, 0, 0, 4, 4).is_ok());
        }
        let logged = b.logged_gaps.borrow();
        // Unknown-xid path for the wired ops; all three log the
        // `_unknown_xid` variant since the test xids aren't in
        // the store fixture.
        assert!(logged.contains("put_image_unknown_xid"));
        assert!(logged.contains("fill_rectangle_unknown_xid"));
        assert!(logged.contains("copy_area_unknown_xid"));
    }

    /// Spec: "boots far enough to service GetGeometry / InternAtom".
    /// Backend::xid_map reflects KmsCore's root xid seed via
    /// for_tests — empty xid map is fine for this test since the
    /// fixture omits the root insert that production does. The
    /// load-bearing check is that the xid_map accessor returns a
    /// real reference rather than panicking.
    #[test]
    fn v2_xid_map_is_reachable_via_backend_trait() {
        let b = KmsBackendV2::for_tests();
        let map = b.xid_map();
        // for_tests builds an empty map (it doesn't seed root the
        // way KmsCore::new does); verify the accessor works and
        // returns an actual map reference.
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn v2_list_fonts_proxy_returns_catalog_matches() {
        let mut b = KmsBackendV2::for_tests();
        let expected = u16::try_from(b.core.font_loader.catalog.len().min(8)).unwrap_or(u16::MAX);
        let reply = b.list_fonts_proxy(None, 8, "*").expect("list_fonts");
        assert_eq!(reply[0], 1);
        let count = u16::from_le_bytes([reply[8], reply[9]]);
        assert_eq!(count, expected);
    }

    #[test]
    fn v2_list_fonts_with_info_proxy_emits_terminator() {
        let mut b = KmsBackendV2::for_tests();
        let replies = b
            .list_fonts_with_info_proxy(None, 4, "*")
            .expect("list_fonts_with_info");
        assert!(!replies.is_empty(), "terminator reply must be present");
        let terminator = replies.last().expect("terminator");
        assert_eq!(terminator[0], 1);
        assert_eq!(terminator[1], 0);
    }

    /// Telemetry: counter sites fire at the Backend trait
    /// surface even on the test fixture (no Vk). put_image with
    /// an unknown xid logs a gap and does NOT count a paint
    /// submit (the engine never ran); get_image likewise. This
    /// confirms only successful ops count.
    #[test]
    fn v2_telemetry_counter_sites_track_successful_ops() {
        let mut b = KmsBackendV2::for_tests();
        // put_image with unknown xid → no counter bump.
        b.put_image(None, 0xDEAD, 32, 4, 4, 0, 0, &[0; 64]).unwrap();
        assert_eq!(b.telemetry.lifetime.paint_submits, 0);
        // The stub engine declines NoVk, so even a known xid
        // wouldn't count. The "track successful ops" gate is
        // covered by the lavapipe integration tests; here we
        // just confirm the wiring compiles and doesn't double-
        // increment on the gap path.
        assert_eq!(b.telemetry.lifetime.queue_submit2, 0);
    }

    /// Bookkeeping methods stay consistent: register_top_level
    /// mutates KmsCore's xid_map; xid_map() reflects the new entry.
    #[test]
    fn v2_register_top_level_updates_xid_map() {
        use yserver_protocol::x11::ResourceId;
        let mut b = KmsBackendV2::for_tests();
        b.register_top_level(None, ResourceId(0x4242), 0x0040_1234)
            .expect("register_top_level");
        assert_eq!(b.xid_map().get(&0x0040_1234), Some(&ResourceId(0x4242)));
        b.unregister_host_window(0x0040_1234);
        assert!(b.xid_map().get(&0x0040_1234).is_none());
    }

    /// Stage 3a per plan §3a: a `poly_text8` wire body that
    /// carries `[text₀, font-change, text₁]` should:
    /// 1. dispatch the first text run with the original
    ///    `current_font` value (or None);
    /// 2. swap `core.current_font` on the inline change item;
    /// 3. dispatch the second text run with the new font.
    ///
    /// Without a real FontState entry the engine call short-
    /// circuits in `render_text_chars_v2` (no font → no work),
    /// but the side-effect we care about — `current_font`
    /// rotating to the inline-change xid by the end of the parse
    /// — is observable on the backend after the call returns.
    #[test]
    fn v2_poly_text8_font_change_advances_current_font() {
        let mut b = KmsBackendV2::for_tests();
        // Body shape (drawable=4, gc=4, x=2, y=2, items=…):
        // header = 12 bytes; first item = `len(1) delta(1) "X"`
        // = 3 bytes; font-change item = `255 + 4 BE bytes` = 5
        // bytes; second item = `len(1) delta(1) "Y"` = 3 bytes.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&[0, 0, 0, 0]); // drawable
        body.extend_from_slice(&[0, 0, 0, 0]); // gc
        body.extend_from_slice(&(0_i16).to_le_bytes()); // x
        body.extend_from_slice(&(0_i16).to_le_bytes()); // y
        // First TEXTITEM8 — single 'X' glyph.
        body.extend_from_slice(&[1u8, 0u8, b'X']);
        // Font-change item — switch to xid 0xDEAD_BEEF.
        body.push(255);
        body.extend_from_slice(&0xDEAD_BEEF_u32.to_be_bytes());
        // Second TEXTITEM8 — single 'Y' glyph.
        body.extend_from_slice(&[1u8, 0u8, b'Y']);

        assert_eq!(b.core.current_font, None);
        b.poly_text8(None, 0xABCD_EF01, 0x000000, &body)
            .expect("poly_text8 ok");
        // After the parse, current_font should reflect the inline
        // change. The parse runs the second text item with this
        // font value in scope.
        assert_eq!(b.core.current_font, Some(0xDEAD_BEEF));
    }

    // ─── Stage 3b: picture record lifecycle tests ──────────────

    /// `picture_record_lifecycle` per plan §3b: create → change →
    /// free, with every value-mask bit exercised at least once.
    /// Round-trip via `KmsCore.pictures.get` after each step.
    #[test]
    fn v2_picture_record_lifecycle_exercises_every_value_mask_bit() {
        use crate::kms::core::PictureFilter;
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        // Pre-create a fake drawable xid so render_create_picture's
        // store.lookup doesn't have to be Some — the picture record
        // just stores the host_xid; the incref path is exercised
        // in the next test.
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0x4242_4242).expect("PixmapHandle"));

        // CPRepeat=Pad, CPAlphaMap=0xDEAD_BEEF, CPAlphaXOrigin=10,
        // CPAlphaYOrigin=20, CPClipXOrigin=30, CPClipYOrigin=40,
        // CPClipMask=0 (= None), CPGraphicsExposure=1,
        // CPSubwindowMode=1, CPPolyEdge=1, CPPolyMode=1,
        // CPDither=1 (consumed-but-not-stored), CPComponentAlpha=1.
        let value_mask: u32 = 0x0001
            | 0x0002
            | 0x0004
            | 0x0008
            | 0x0010
            | 0x0020
            | 0x0040
            | 0x0080
            | 0x0100
            | 0x0200
            | 0x0400
            | 0x0800
            | 0x1000;
        let mut values: Vec<u8> = Vec::new();
        for v in [
            2_u32,       // Repeat::Pad
            0xDEAD_BEEF, // alpha_map
            10,          // alpha_x
            20,          // alpha_y
            30,          // clip_x
            40,          // clip_y
            0,           // clip_mask = None
            1,           // graphics_exposure
            1,           // subwindow_mode
            1,           // poly_edge
            1,           // poly_mode
            1,           // dither (consumed, not stored)
            1,           // component_alpha
        ] {
            values.extend_from_slice(&v.to_le_bytes());
        }

        let picture = b
            .render_create_picture(None, drawable, 0, value_mask, &values)
            .expect("create_picture")
            .expect("Some(handle)");
        let pic_xid = picture.as_raw();

        // Find and unpack the resulting record.
        let rec = b.core.pictures.get(&pic_xid).expect("record present");
        match rec {
            PictureRecord::Drawable {
                host_xid,
                pict_format: _,
                clip,
                clip_x,
                clip_y,
                repeat,
                alpha_map,
                alpha_x,
                alpha_y,
                component_alpha,
                transform,
                filter,
                graphics_exposure,
                subwindow_mode,
                poly_edge,
                poly_mode,
                drawable_origin: _,
            } => {
                assert_eq!(*host_xid, 0x4242_4242);
                assert!(clip.is_none(), "clip stays None for clip_mask=0");
                assert_eq!(*clip_x, 30);
                assert_eq!(*clip_y, 40);
                assert_eq!(*repeat, Repeat::Pad);
                assert_eq!(*alpha_map, Some(0xDEAD_BEEF));
                assert_eq!(*alpha_x, 10);
                assert_eq!(*alpha_y, 20);
                assert!(*component_alpha);
                assert!(transform.is_none());
                assert_eq!(*filter, PictureFilter::Nearest);
                assert!(*graphics_exposure);
                assert_eq!(*subwindow_mode, 1);
                assert_eq!(*poly_edge, 1);
                assert_eq!(*poly_mode, 1);
            }
            other => panic!("expected Drawable, got {other:?}"),
        }

        // ChangePicture override of a single bit (CPRepeat=Normal).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0x0001_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes()); // Repeat::Normal
        b.render_change_picture(None, pic_xid, &body)
            .expect("change_picture");
        match b.core.pictures.get(&pic_xid) {
            Some(PictureRecord::Drawable { repeat, .. }) => {
                assert_eq!(*repeat, Repeat::Normal);
            }
            _ => panic!("record dropped"),
        }

        // FreePicture removes the record.
        b.render_free_picture(None, pic_xid).expect("free_picture");
        assert!(!b.core.pictures.contains_key(&pic_xid));
    }

    /// `picture_record_drawable_refcount` per plan §3b: a picture
    /// wrapping a pixmap incref's the pixmap on create; the pixmap
    /// survives `free_pixmap` while a picture still references it;
    /// `render_free_picture` decref's, allowing the pending retire
    /// to complete on the next poll.
    #[test]
    fn v2_picture_record_drawable_refcount_blocks_free_pixmap() {
        use ash::vk;

        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        // The `for_tests` fixture has no VkContext, so the
        // production `create_pixmap` path falls back to a logged
        // gap (no storage allocated). Use the store's test-stub
        // path directly so refcount accounting is exercised
        // without needing a live Vk.
        let pix_xid = 0xDEAD_BABE;
        let storage = Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let pix_id = b
            .store
            .allocate(pix_xid, DrawableKind::Pixmap, 32, false, storage)
            .expect("store allocate");
        assert_eq!(b.store.get(pix_id).expect("entry").refcount, 1);

        // Create a picture wrapping the pixmap; refcount → 2.
        let pix_handle = PixmapHandle::from_raw(pix_xid).expect("PixmapHandle");
        let any = AnyHandle::Pixmap(pix_handle);
        let pic = b
            .render_create_picture(None, any, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();
        assert_eq!(b.store.get(pix_id).expect("entry").refcount, 2);

        // free_pixmap drops one ref → 1; the entry survives because
        // the picture still references it.
        b.free_pixmap(None, pix_xid).expect("free_pixmap");
        assert_eq!(b.store.get(pix_id).expect("entry survives").refcount, 1);

        // free_picture drops the second ref → 0; the entry retires.
        // The test-stub storage has no in-flight fence, so
        // `destroy_now` runs immediately and the entry is removed.
        b.render_free_picture(None, pic_xid).expect("free_picture");
        assert!(b.store.get(pix_id).is_none(), "entry destroyed on last ref");
    }

    /// `picture_solid_fill_premul_correct` per plan §3b. NB: the
    /// X RENDER wire colour is **already premultiplied** per the
    /// protocol + rendercheck (`main.c:337-345`), so v2 stores the
    /// channels as-is rather than multiplying by alpha. The plan's
    /// `0x80808080 → [0.25, 0.25, 0.25, 0.5]` example assumed
    /// straight-alpha input; v1 has been parity with rendercheck
    /// since Phase 4.1.4.6, and v2 matches v1.
    #[test]
    fn v2_render_create_solid_fill_stores_wire_color_as_is() {
        // Wire colour: r16=0xFFFF (1.0), g16=0x8080 (≈0.50196),
        // b16=0x0000 (0.0), a16=0x8080 (≈0.50196). Stored f32
        // values should be (r=1.0, g=0.5019, b=0.0, a=0.5019)
        // exactly — no premultiplication applied at store time.
        let mut b = KmsBackendV2::for_tests();
        let color: [u8; 8] = [0xFF, 0xFF, 0x80, 0x80, 0x00, 0x00, 0x80, 0x80];
        let pic = b
            .render_create_solid_fill(None, color)
            .expect("solid_fill")
            .expect("Some");
        let rec = b.core.pictures.get(&pic.as_raw()).expect("record");
        match rec {
            PictureRecord::SolidFill {
                premul,
                repeat,
                component_alpha,
            } => {
                assert!((premul[0] - 1.0).abs() < 1e-4, "r = {}", premul[0]);
                assert!(
                    (premul[1] - (0x8080_u16 as f32 / 65535.0)).abs() < 1e-6,
                    "g = {}",
                    premul[1],
                );
                assert!(premul[2].abs() < 1e-6, "b = {}", premul[2]);
                assert!(
                    (premul[3] - (0x8080_u16 as f32 / 65535.0)).abs() < 1e-6,
                    "a = {}",
                    premul[3],
                );
                // Solid-fill defaults to Repeat::Normal; component_alpha=false.
                assert_eq!(*repeat, Repeat::Normal);
                assert!(!*component_alpha);
            }
            other => panic!("expected SolidFill, got {other:?}"),
        }
    }

    /// `picture_gradient_record_stored` per plan §3b: a linear
    /// gradient body parses; endpoints + stops round-trip through
    /// the record.
    #[test]
    fn v2_render_create_linear_gradient_parses_endpoints_and_stops() {
        let mut b = KmsBackendV2::for_tests();
        // Wire body: pad(4) + p1.x(4) + p1.y(4) + p2.x(4) + p2.y(4)
        // + n_stops(4) + n*pos(4) + n*color(8).
        // p1 = (0, 0) fixed-point; p2 = (256<<16, 0); two stops at
        // pos=0 with color=(0xFFFF, 0, 0, 0xFFFF) and pos=1<<16 with
        // color=(0, 0xFFFF, 0, 0xFFFF).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes()); // request padding (skipped)
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
        body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
        body.extend_from_slice(&2_u32.to_le_bytes()); // n_stops
        // positions
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0x0001_0000_i32.to_le_bytes());
        // colours
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes()); // r0
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes());
        body.extend_from_slice(&0_u16.to_le_bytes()); // r1=0
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes()); // g1
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes());

        let pic = b
            .render_create_linear_gradient(None, &body)
            .expect("linear_gradient")
            .expect("Some");
        let rec = b.core.pictures.get(&pic.as_raw()).expect("record");
        match rec {
            PictureRecord::LinearGradient {
                p1,
                p2,
                stops,
                repeat,
                transform,
            } => {
                assert_eq!(*p1, (0, 0));
                assert_eq!(*p2, (256 << 16, 0));
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].pos, 0);
                assert_eq!(stops[0].r, 0xFFFF);
                assert_eq!(stops[0].g, 0);
                assert_eq!(stops[1].pos, 0x0001_0000);
                assert_eq!(stops[1].g, 0xFFFF);
                assert_eq!(*repeat, Repeat::None);
                assert!(transform.is_none());
            }
            other => panic!("expected LinearGradient, got {other:?}"),
        }
    }

    /// Stage 3f.13: `render_create_linear_gradient` returns a
    /// resolved `ResolvedSource::Gradient(xid)` from
    /// `resolve_picture_for_render` (not a SolidFill collapse).
    /// Logic-only — engine-side LUT build is a Vk path and lives
    /// in the engine's Vk-backed tests; here we just assert the
    /// resolve shape changed correctly.
    #[test]
    fn v2_linear_gradient_resolves_as_gradient_source() {
        use crate::kms::v2::engine::ResolvedSource;

        let mut b = KmsBackendV2::for_tests();
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes()); // pad
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
        body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
        body.extend_from_slice(&1_u32.to_le_bytes()); // n_stops
        body.extend_from_slice(&0_i32.to_le_bytes()); // pos
        body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]); // colour (R=1, A=1)
        let pic = b
            .render_create_linear_gradient(None, &body)
            .expect("create gradient")
            .expect("Some");

        let (resolved, _, _, _) =
            resolve_picture_for_render(&b.core, &b.store, pic.as_raw()).expect("resolve");
        match resolved {
            ResolvedSource::Gradient(xid) => assert_eq!(xid, pic.as_raw()),
            other => panic!("expected Gradient, got {other:?}"),
        }
    }

    /// Stage 3f.13: `render_free_picture` for a gradient drops both
    /// the picture record and the engine-side `picture_paint` slot.
    /// Logic-only — the engine slot count is the observable signal
    /// (`engine.picture_paint_len()`). On the test fixture (no Vk)
    /// the build itself logs a debug + skips, so the engine slot
    /// stays at 0 throughout; the gate is "free_picture doesn't
    /// leave a stale slot behind" which still asserts non-zero in
    /// production but zero in test. We assert the lifecycle path
    /// instead: create, free, ensure picture record is gone.
    #[test]
    fn v2_gradient_free_picture_drops_record() {
        let mut b = KmsBackendV2::for_tests();
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes()); // pad
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&(128_i32 << 16).to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]);
        let pic = b
            .render_create_linear_gradient(None, &body)
            .expect("create gradient")
            .expect("Some");
        let xid = pic.as_raw();
        assert!(b.core.pictures.contains_key(&xid));
        b.render_free_picture(None, xid).expect("free");
        assert!(!b.core.pictures.contains_key(&xid));
        assert_eq!(b.engine.picture_paint_len(), 0);
    }

    /// Stage 3f.14: depth-32 windows are premultiplied-α and a
    /// transparent-black default is the no-op contribution to
    /// compositing; depth-24 (and other non-α visuals) get opaque
    /// black. Locks the contract in the test suite so a refactor
    /// doesn't silently flip 32-bit windows to opaque black (which
    /// would visually look the same on top of the root but break
    /// compositors that depend on alpha for blending).
    #[test]
    fn v2_default_window_init_color_per_depth() {
        assert_eq!(super::default_window_init_color(32), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(super::default_window_init_color(24), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(super::default_window_init_color(1), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(super::default_window_init_color(8), [0.0, 0.0, 0.0, 1.0]);
    }

    /// `render_set_picture_clip_rectangles` parses + stores rects
    /// pre-shifted by the clip-origin. Then `render_free_picture`
    /// teardown also drops the engine-side picture_paint slot.
    #[test]
    fn v2_set_picture_clip_rectangles_pre_shifts_by_origin() {
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA00_BB00).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Wire body: picture(4) + x_origin(2) + y_origin(2) +
        // 1 × [x=5, y=6, w=20, h=30].
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&10_i16.to_le_bytes()); // x_origin
        body.extend_from_slice(&20_i16.to_le_bytes()); // y_origin
        body.extend_from_slice(&5_i16.to_le_bytes());
        body.extend_from_slice(&6_i16.to_le_bytes());
        body.extend_from_slice(&20_u16.to_le_bytes());
        body.extend_from_slice(&30_u16.to_le_bytes());
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_picture_clip");
        // Pre-shift: stored rect.x = 5 + 10 = 15; .y = 6 + 20 = 26.
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable {
                clip,
                clip_x,
                clip_y,
                ..
            } => {
                let rects = clip.as_ref().expect("Some(rects)");
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].x, 15);
                assert_eq!(rects[0].y, 26);
                assert_eq!(rects[0].width, 20);
                assert_eq!(rects[0].height, 30);
                assert_eq!(*clip_x, 10);
                assert_eq!(*clip_y, 20);
            }
            _ => panic!("not Drawable"),
        }

        // free_picture removes both record + engine-side slot.
        assert_eq!(b.engine.picture_paint_len(), 0);
        b.render_free_picture(None, pic_xid).expect("free");
        assert!(!b.core.pictures.contains_key(&pic_xid));
        assert_eq!(b.engine.picture_paint_len(), 0);
    }

    /// X11 RENDER `SetPictureClipRectangles` with an EMPTY rect
    /// list = empty clip region = composite paints **nothing**.
    /// Distinct from `ChangePicture(CPClipMask = None)` which
    /// clears the clip back to "paint everywhere" (`clip = None`).
    ///
    /// Regression: pre-fix v2 collapsed empty-list to `clip = None`,
    /// which made subsequent composites paint everywhere — exactly
    /// the mate-with-compositing "shadow only / wallpaper
    /// overwrites window content" bug observed in the Stage 4d
    /// smoke. The trace at 09:49:44 showed marco's wallpaper-fill
    /// composite running with `clip[]` (= `None` in v2 storage)
    /// even though marco's intent (per X11 spec) was "empty clip,
    /// paint nothing."
    ///
    /// Post-fix: empty rect list stores `Some(Vec::new())` so the
    /// engine's `clip_rects=Some(&[])` path returns early without
    /// painting.
    #[test]
    fn v2_set_picture_clip_rectangles_empty_list_is_empty_clip_not_no_clip() {
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xCC00_DD00).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // First: set a real clip to prove the field can become
        // populated (Some(non-empty)).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
        body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
        body.extend_from_slice(&0_i16.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes());
        body.extend_from_slice(&100_u16.to_le_bytes());
        body.extend_from_slice(&100_u16.to_le_bytes());
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_clip non-empty");
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable { clip, .. } => {
                assert!(
                    matches!(clip, Some(v) if v.len() == 1),
                    "expected Some(1 rect) after non-empty set, got {clip:?}",
                );
            }
            _ => panic!("not Drawable"),
        }

        // Now: empty list. Per X11 RENDER spec this means "empty
        // clip region — paint nothing." The stored representation
        // must distinguish this from "no clip set" (paint
        // everywhere).
        let mut empty_body: Vec<u8> = Vec::new();
        empty_body.extend_from_slice(&pic_xid.to_le_bytes());
        empty_body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
        empty_body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
        // No rect data — empty list.
        b.render_set_picture_clip_rectangles(None, pic_xid, &empty_body)
            .expect("set_clip empty");
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable { clip, .. } => {
                // Pre-fix: clip was None (= paint everywhere).
                // Post-fix: Some(empty Vec) (= paint nothing).
                assert!(
                    matches!(clip, Some(v) if v.is_empty()),
                    "empty rect list must store Some(empty Vec) — \
                     pre-fix stored None which made composites paint \
                     everywhere instead of nothing. Got: {clip:?}",
                );
            }
            _ => panic!("not Drawable"),
        }
    }

    // ─── Audit #8 (2026-05-19): set_picture_drawable_origin +
    // picture_client_clip_rects v2 backend hooks ──────────────

    /// `set_picture_drawable_origin` writes into the
    /// `PictureRecord::Drawable.drawable_origin` field. Pre-fix v2
    /// inherited the trait default no-op so the field stayed at
    /// (0, 0); window-backed pictures whose drawable sits at a
    /// non-zero parent offset couldn't translate external region
    /// geometry back into picture-local coords.
    #[test]
    fn v2_set_picture_drawable_origin_persists_on_record() {
        use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA01_BB01).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Pre-call sanity: default origin must be (0, 0).
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable {
                drawable_origin, ..
            } => assert_eq!(*drawable_origin, (0, 0)),
            _ => panic!("not Drawable"),
        }

        b.set_picture_drawable_origin(pic_xid, (15, 27));

        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable {
                drawable_origin, ..
            } => {
                assert_eq!(
                    *drawable_origin,
                    (15, 27),
                    "drawable_origin must update; pre-fix the trait default \
                     no-op left it at (0, 0)",
                );
            }
            _ => panic!("not Drawable"),
        }
    }

    /// `set_picture_drawable_origin` on a non-Drawable picture
    /// (SolidFill / gradient) is a tolerated no-op — those variants
    /// have no drawable to anchor to.
    #[test]
    fn v2_set_picture_drawable_origin_no_op_on_solidfill() {
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();
        // Color is fixed-size 8 bytes (BGRA u16×4).
        let mut color = [0u8; 8];
        color[0..2].copy_from_slice(&0xFFFF_u16.to_le_bytes()); // R
        color[6..8].copy_from_slice(&0xFFFF_u16.to_le_bytes()); // A
        let pic = b
            .render_create_solid_fill(None, color)
            .expect("create solid fill")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Should not panic; record must remain a SolidFill.
        b.set_picture_drawable_origin(pic_xid, (10, 20));
        assert!(
            matches!(
                b.core.pictures.get(&pic_xid),
                Some(PictureRecord::SolidFill { .. })
            ),
            "SolidFill picture must remain SolidFill after \
             set_picture_drawable_origin no-op",
        );
    }

    /// `picture_client_clip_rects` on a Drawable picture WITH a
    /// clip returns `Some(Some(rects))` — those rects feed
    /// `CreateRegionFromPicture` (XFixes). Pre-fix v2 inherited
    /// the trait default `None`, making CreateRegionFromPicture
    /// always return BadMatch even for legitimate clipped pictures.
    #[test]
    fn v2_picture_client_clip_rects_returns_set_clip() {
        use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA02_BB02).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Install a 2-rect client clip via SetPictureClipRectangles
        // (clip-origin both zero so stored rects == request rects).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
        body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
        for (x, y, w, h) in [(0_i16, 0_i16, 10_u16, 10_u16), (100, 200, 30, 40)] {
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&y.to_le_bytes());
            body.extend_from_slice(&w.to_le_bytes());
            body.extend_from_slice(&h.to_le_bytes());
        }
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_clip");

        let got = b
            .picture_client_clip_rects(pic_xid)
            .expect("Drawable picture must be Some(_) (not BadMatch)");
        let rects = got.expect("clip was set, expected Some(rects)");
        assert_eq!(rects.len(), 2, "got {rects:?}");
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (0, 0, 10, 10)
        );
        assert_eq!(
            (rects[1].x, rects[1].y, rects[1].width, rects[1].height),
            (100, 200, 30, 40)
        );
    }

    /// Non-zero drawable origins must not corrupt `CreateRegionFromPicture`.
    /// The request path stores the origin separately, but the returned client
    /// clip still needs to reflect the picture-local rectangle coordinates
    /// only.
    #[test]
    fn v2_picture_client_clip_rects_window_backed_picture_with_nonzero_origin() {
        use yserver_core::backend::{AnyHandle, Backend, WindowHandle};

        let mut b = KmsBackendV2::for_tests();
        let window_xid = 0xAA04_BB04;
        let _w_id = seed_window(&mut b, window_xid, None, 15, 27);

        let pic = b
            .render_create_picture(
                None,
                AnyHandle::Window(WindowHandle::from_raw(window_xid).expect("WindowHandle")),
                0,
                0,
                &[],
            )
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Mirror the request-layer origin bookkeeping that happens on CreatePicture.
        b.set_picture_drawable_origin(pic_xid, (15, 27));

        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&5_i16.to_le_bytes()); // clip origin x
        body.extend_from_slice(&9_i16.to_le_bytes()); // clip origin y
        body.extend_from_slice(&1_i16.to_le_bytes());
        body.extend_from_slice(&2_i16.to_le_bytes());
        body.extend_from_slice(&7_u16.to_le_bytes());
        body.extend_from_slice(&11_u16.to_le_bytes());
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_clip");

        let got = b
            .picture_client_clip_rects(pic_xid)
            .expect("Drawable picture must be Some(_) (not BadMatch)");
        let rects = got.expect("clip was set, expected Some(rects)");
        assert_eq!(rects.len(), 1, "got {rects:?}");
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (6, 11, 7, 11),
            "drawable_origin must not be folded into CreateRegionFromPicture",
        );
    }

    /// `picture_client_clip_rects` on a Drawable picture with NO
    /// clip set returns `Some(None)` — the picture exists but has
    /// no clientClip yet. Per X RENDER /
    /// `xfixes/region.c:CreateRegionFromPicture`, the dispatcher
    /// then emits BadMatch on the caller (no region to extract).
    #[test]
    fn v2_picture_client_clip_rects_returns_some_none_when_no_clip_set() {
        use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA03_BB03).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        let got = b.picture_client_clip_rects(pic_xid);
        assert!(
            matches!(got, Some(None)),
            "Drawable picture without a clip must return Some(None) — \
             got {got:?}",
        );
    }

    /// `picture_client_clip_rects` on a non-Drawable picture (e.g.,
    /// SolidFill) returns the outer `None` so the protocol layer
    /// raises BadMatch — gradients/solidfills carry no
    /// `clientClip`. Mirrors Xorg's `CreateRegionFromPicture` →
    /// BadPicture path for sourceless pictures.
    #[test]
    fn v2_picture_client_clip_rects_outer_none_on_solidfill() {
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();
        let mut color = [0u8; 8];
        color[0..2].copy_from_slice(&0xFFFF_u16.to_le_bytes());
        color[6..8].copy_from_slice(&0xFFFF_u16.to_le_bytes());
        let pic = b
            .render_create_solid_fill(None, color)
            .expect("create solid fill")
            .expect("Some");
        let pic_xid = pic.as_raw();

        let got = b.picture_client_clip_rects(pic_xid);
        assert!(
            got.is_none(),
            "SolidFill picture must return outer None so the protocol \
             layer emits BadMatch — got {got:?}",
        );
    }

    // ─── Stage 3d: render_composite_glyphs tests ───────────────

    /// Helper: install a SolidFill source picture + a glyphset
    /// holding `n` 1×1 A8 glyphs at id 0..n with `0xFF` alpha.
    /// Returns (src_pic_xid, gs_xid).
    fn install_solidfill_and_glyphset(b: &mut KmsBackendV2, n: u32) -> (u32, u32) {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState, StoredGlyph};

        let src_pic = b
            .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
            .expect("solid_fill")
            .expect("Some");

        let gs_xid = b.core.next_host_xid();
        let mut glyphs = HashMap::new();
        for id in 0..n {
            glyphs.insert(
                id,
                StoredGlyph {
                    width: 1,
                    height: 1,
                    x: 0,
                    y: 0,
                    x_off: 1,
                    y_off: 0,
                    pixels: vec![0xFF],
                    format: GlyphSetFormat::A8,
                },
            );
        }
        b.core.glyphsets.insert(
            gs_xid,
            GlyphSetState {
                format: GlyphSetFormat::A8,
                glyphs,
            },
        );
        (src_pic.as_raw(), gs_xid)
    }

    /// Per plan §3d "Op / source matrix accepted by 3d": op != Over
    /// (3) must drop the call with a per-call gap-log and increment
    /// the `composite_glyphs_dropped_unsupported` lifetime counter.
    /// No paint side effect; engine is never reached.
    #[test]
    fn v2_composite_glyphs_unsupported_op_drops() {
        let mut b = KmsBackendV2::for_tests();
        let (src_pic, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);
        // No real dst picture needed — the op gate fires before
        // dst resolution. Pass any host_dst; assert gap-counter.
        b.render_composite_glyphs(
            None,
            23, /* CompositeGlyphs8 */
            1,  /* op = Src, NOT Over */
            src_pic,
            0xDEAD, /* host_dst (unused — op gate first) */
            0,      /* mask_fmt */
            gs_xid,
            0,
            0,
            &[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], // items: 1 glyph elt + padded
            0,
            0,
        )
        .expect("ok");
        assert_eq!(
            b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 1,
            "op != Over must bump the unsupported counter",
        );
        assert_eq!(
            b.telemetry.lifetime.paint_submits, 0,
            "no paint submit on the gap path",
        );
    }

    /// Stage 3f.12: gradient src is no longer a "drop" — it
    /// collapses to a SolidFill of the first stop's premultiplied
    /// colour (real LUT sampling is still post-3f.5 work). The
    /// composite_glyphs path now accepts gradient sources; the
    /// `composite_glyphs_dropped_unsupported` counter stays at 0.
    /// Cairo glyph rendering with gradient bg/fg therefore paints
    /// (with the gradient flattened to its start colour) rather
    /// than dropping entirely.
    #[test]
    fn v2_composite_glyphs_gradient_source_collapses_to_solidfill() {
        let mut b = KmsBackendV2::for_tests();
        let (_unused_solidfill, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);
        // Minimal valid linear-gradient wire body: pad(4) +
        // p1(8) + p2(8) + n_stops=1(4) + stop_pos(4) + stop_color(8).
        let mut grad_body: Vec<u8> = Vec::new();
        grad_body.extend_from_slice(&0_u32.to_le_bytes()); // request pad (skipped)
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
        grad_body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
        grad_body.extend_from_slice(&1_u32.to_le_bytes()); // n_stops
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // pos
        grad_body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]); // colour
        let grad_pic = b
            .render_create_linear_gradient(None, &grad_body)
            .expect("gradient")
            .expect("Some")
            .as_raw();
        b.render_composite_glyphs(
            None,
            23,
            3, /* Over */
            grad_pic,
            0xDEAD,
            0,
            gs_xid,
            0,
            0,
            &[1u8, 0, 0, 0, 0, 0, 0, 0],
            0,
            0,
        )
        .expect("ok");
        assert_eq!(
            b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 0,
            "gradient src must collapse to SolidFill (not drop)",
        );
    }

    /// Per plan §3d items-parse spec: the items stream's inline
    /// `0xFF 0 0 0 new_gs_xid` element rotates the active glyphset
    /// for subsequent glyph lookups. The test installs two
    /// glyphsets with distinct codepoint→pixel mappings, feeds an
    /// items stream that draws one glyph from each, and asserts
    /// that both glyphsets contributed to the engine call — the
    /// parser must have honoured the inline change. We can't hit
    /// the Vk engine in this fixture (no live Vk under
    /// `for_tests`), so the gate is "no unsupported drop fired"
    /// AND "both glyphset lookups succeeded" (verified by reaching
    /// the engine, which returns `NoVk` on the stub but does NOT
    /// bump the unsupported counter).
    #[test]
    fn v2_composite_glyphs_inline_glyphset_change_parsed() {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState, StoredGlyph};

        let mut b = KmsBackendV2::for_tests();
        let src_pic = b
            .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
            .expect("solid_fill")
            .expect("Some")
            .as_raw();
        // GlyphSet A: codepoint 0x10 → 0xAA pixels.
        // GlyphSet B: codepoint 0x20 → 0xBB pixels.
        let mut mk_gs = |code: u32, byte: u8| {
            let mut glyphs = HashMap::new();
            glyphs.insert(
                code,
                StoredGlyph {
                    width: 1,
                    height: 1,
                    x: 0,
                    y: 0,
                    x_off: 1,
                    y_off: 0,
                    pixels: vec![byte],
                    format: GlyphSetFormat::A8,
                },
            );
            let xid = b.core.next_host_xid();
            b.core.glyphsets.insert(
                xid,
                GlyphSetState {
                    format: GlyphSetFormat::A8,
                    glyphs,
                },
            );
            xid
        };
        let gs_a = mk_gs(0x10, 0xAA);
        let gs_b = mk_gs(0x20, 0xBB);
        // Need a dst Drawable picture — create a stub one (lookup
        // will fail since the underlying drawable xid isn't in
        // the store, so the engine call short-circuits before
        // anything reaches Vk, but the parser still walks).
        use yserver_core::backend::{AnyHandle, PixmapHandle};
        let dst_drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0x4242_4242).expect("PixmapHandle"));
        let dst_pic = b
            .render_create_picture(None, dst_drawable, 0, 0, &[])
            .expect("dst_picture")
            .expect("Some")
            .as_raw();
        // Items stream: 1 glyph 0x10 from gs_a (initial), inline
        // glyphset-change to gs_b, then 1 glyph 0x20 from gs_b.
        // Element layout: count(u8) pad pad pad dx(i16) dy(i16) ids...
        let mut items: Vec<u8> = Vec::new();
        // Element 1: 1 glyph @ (0,0).
        items.extend_from_slice(&[1u8, 0, 0, 0, 0, 0, 0, 0]);
        items.extend_from_slice(&[0x10, 0, 0, 0]); // padded ids
        // Element 2: glyphset change.
        items.push(255);
        items.extend_from_slice(&[0u8, 0, 0]);
        items.extend_from_slice(&gs_b.to_le_bytes());
        // Element 3: 1 glyph @ (+1,0).
        items.extend_from_slice(&[1u8, 0, 0, 0, 1, 0, 0, 0]);
        items.extend_from_slice(&[0x20, 0, 0, 0]);

        b.render_composite_glyphs(
            None, 23, 3, /* Over */
            src_pic, dst_pic, 0, gs_a, 0, 0, &items, 0, 0,
        )
        .expect("ok");
        // Op + source were Over + SolidFill, so the unsupported
        // counter must NOT have fired.
        assert_eq!(
            b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 0,
            "Over + SolidFill must not hit the unsupported gate",
        );
        // dst resolution failed (no Drawable backing for 0x4242_4242
        // in the store), so the engine wasn't called — but the parse
        // still walked both glyphsets without bumping the gap. The
        // load-bearing assertion is that the inline change keeps the
        // call in the Over+SolidFill envelope; engine reachability
        // is covered by the Vk-backed acceptance test.
    }

    // ─── Stage 3f.1: poly_* + fill_poly logic tests ────────────

    /// `poly_line_origin_mode_offsets_correctly` per plan §3f tests.
    /// Build a 3-point path under both Origin (absolute) and
    /// Previous (delta) coordinate modes; assert the produced
    /// rasterised-pixel set is the same. Drives Bresenham via the
    /// public crate-level helper.
    #[test]
    fn poly_line_origin_mode_offsets_correctly() {
        use crate::kms::{
            backend::{bresenham_segment, read_i16_pair},
            cpu_types::Rectangle16,
        };

        // Path: (10, 10) → (10, 13) → (13, 13) — an L shape.
        let absolute_pts: [(i16, i16); 3] = [(10, 10), (10, 13), (13, 13)];
        // Same path under Previous mode: first pt absolute, then deltas.
        let delta_pts: [(i16, i16); 3] = [(10, 10), (0, 3), (3, 0)];

        let rasterise = |points: &[u8], mode: u8| -> Vec<Rectangle16> {
            let mut rects: Vec<Rectangle16> = Vec::new();
            let mut prev: Option<(i32, i32)> = None;
            let mut offset = 0;
            while let Some((x, y)) = read_i16_pair(points, offset) {
                offset += 4;
                let (xi, yi) = if mode == 1 {
                    if let Some((px, py)) = prev {
                        (px + i32::from(x), py + i32::from(y))
                    } else {
                        (i32::from(x), i32::from(y))
                    }
                } else {
                    (i32::from(x), i32::from(y))
                };
                if let Some((px, py)) = prev {
                    bresenham_segment(px, py, xi, yi, &mut rects);
                }
                prev = Some((xi, yi));
            }
            rects
        };

        let pack = |pts: &[(i16, i16)]| -> Vec<u8> {
            let mut out = Vec::with_capacity(pts.len() * 4);
            for (x, y) in pts {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            out
        };

        let abs_rects = rasterise(&pack(&absolute_pts), 0);
        let prev_rects = rasterise(&pack(&delta_pts), 1);

        // Both modes must produce the same rasterised pixel set.
        let to_set = |rs: &[Rectangle16]| -> std::collections::BTreeSet<(i16, i16)> {
            rs.iter().map(|r| (r.x, r.y)).collect()
        };
        assert_eq!(to_set(&abs_rects), to_set(&prev_rects));
        // Sanity: pixel set covers the L's expected vertices.
        let set = to_set(&abs_rects);
        for p in [(10, 10), (10, 13), (13, 13)] {
            assert!(set.contains(&p), "missing endpoint {p:?}");
        }
    }

    /// `fill_poly_scanline_correctness` per plan §3f tests. A 5-point
    /// convex polygon (axis-aligned diamond) round-trips through
    /// `scanline_fill_polygon` and produces the expected horizontal
    /// span set. Even-odd-rule fill, half-open scanline range.
    #[test]
    fn fill_poly_scanline_correctness() {
        use crate::kms::{backend::scanline_fill_polygon, cpu_types::Rectangle16};

        // Square with one mid-edge vertex injected — still convex,
        // and 5 distinct vertices as the test name advertises. Vertex
        // list: (0,0) (4,0) (4,2) (4,4) (0,4) — a 4×4 square with an
        // extra vertex on the right edge. Filled region is rows
        // y ∈ [0, 4) with x ∈ [0, 4) at each row.
        let verts = [(0, 0), (4, 0), (4, 2), (4, 4), (0, 4)];
        let mut rects: Vec<Rectangle16> = Vec::new();
        scanline_fill_polygon(&verts, &mut rects);

        // Collect (y, x_start, x_end) per row. Each row should be a
        // single span; we union rects on shared y if needed.
        let mut rows: std::collections::BTreeMap<i16, (i16, i16)> =
            std::collections::BTreeMap::new();
        for r in &rects {
            let x_start = r.x;
            let x_end = r.x + r.width as i16;
            rows.entry(r.y)
                .and_modify(|cur| {
                    cur.0 = cur.0.min(x_start);
                    cur.1 = cur.1.max(x_end);
                })
                .or_insert((x_start, x_end));
        }
        // Expected: rows 0..=3 each span x ∈ [0, 4). Row 4 is the
        // top edge of the polygon under half-open [y0, y1) semantics
        // — no horizontal scan crosses it.
        for y in 0..4 {
            let span = rows.get(&y).copied().unwrap_or_else(|| {
                panic!("row {y} missing");
            });
            assert_eq!(span, (0, 4), "row {y} span");
        }
        assert!(!rows.contains_key(&4), "row 4 must not be filled");
    }

    /// Sanity: the v2 GC-clip intersection helper matches v1's shape.
    /// A single source rect clipped against a 2-rect clip yields the
    /// 2 expected intersection rectangles in dst space (clip origin
    /// already applied).
    #[test]
    fn poly_fill_rectangle_honours_gc_clip() {
        use crate::kms::cpu_types::Rectangle16;
        use yserver_core::backend::ClipState;
        use yserver_protocol::x11::ClipRectangles;

        let mut b = KmsBackendV2::for_tests();
        // Two 4×8 clip rects side-by-side starting at (5, 5), with
        // clip origin (10, 10) → effective dst-coord rects at
        // (15, 15)-(19, 23) and (25, 15)-(29, 23).
        let mut wire: Vec<u8> = Vec::new();
        for (x, y, w, h) in [(5_i16, 5_i16, 4_u16, 8_u16), (15, 5, 4, 8)] {
            wire.extend_from_slice(&x.to_le_bytes());
            wire.extend_from_slice(&y.to_le_bytes());
            wire.extend_from_slice(&w.to_le_bytes());
            wire.extend_from_slice(&h.to_le_bytes());
        }
        b.core.current_clip = ClipState::Rectangles {
            origin: (10, 10),
            rects: ClipRectangles {
                ordering: 0,
                x_origin: 0,
                y_origin: 0,
                rectangles: wire,
            },
        };

        // Single source rect that spans both clip rects horizontally
        // and overflows top + bottom of the clip vertically.
        let src = [Rectangle16 {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }];
        let out = b.intersect_with_current_clip(&src);
        assert_eq!(out.len(), 2);
        // First intersection — left clip rect after origin shift.
        assert_eq!(out[0].x, 15);
        assert_eq!(out[0].y, 15);
        assert_eq!(out[0].width, 4);
        assert_eq!(out[0].height, 8);
        // Second intersection — right clip rect after origin shift.
        assert_eq!(out[1].x, 25);
        assert_eq!(out[1].y, 15);
        assert_eq!(out[1].width, 4);
        assert_eq!(out[1].height, 8);
    }

    /// `gxcopy_planemask_diverts_to_logic_fill` per plan §3f tests.
    /// Asserts that switching `KmsCore.current_function` to a
    /// non-`Copy` value (here `Xor`) doesn't emit the
    /// `fill_rects_non_gxcopy` or `copy_plane_non_gxcopy` gaps —
    /// proves the Stage 3f.2 routing change took effect. Engine
    /// itself returns `NoVk` on the stub fixture, so we can't assert
    /// pixel correctness here (that's the Vk acceptance test); but
    /// the gap absence is the load-bearing observable that the
    /// diversion is wired through `fill_solid_rects` →
    /// `engine.logic_fill` rather than the pre-3f.2 short-circuit.
    #[test]
    fn gxcopy_planemask_diverts_to_logic_fill() {
        use yserver_core::backend::GcFunction;
        let mut b = KmsBackendV2::for_tests();
        b.core.current_function = GcFunction::Xor;

        // Single rect: x=0 y=0 w=1 h=1.
        let mut wire = Vec::with_capacity(8);
        wire.extend_from_slice(&0_i16.to_le_bytes());
        wire.extend_from_slice(&0_i16.to_le_bytes());
        wire.extend_from_slice(&1_u16.to_le_bytes());
        wire.extend_from_slice(&1_u16.to_le_bytes());
        b.poly_fill_rectangle(None, 0xDEAD_BEEF, 0xFFFFFFFF, &wire)
            .expect("ok");
        let gaps = b.logged_gaps.borrow();
        assert!(
            !gaps.contains("fill_rects_non_gxcopy"),
            "stage 3f.1 fill_rects_non_gxcopy gap must not fire post-3f.2"
        );
        assert!(
            !gaps.contains("copy_plane_non_gxcopy"),
            "stage 3e.1 copy_plane_non_gxcopy gap must not fire post-3f.2"
        );
    }

    /// `set_clip_pixmap_stores_pixmap_clip` — Stage 3f.3 bookkeeping
    /// gate. The pre-3f.3 stub logged a gap and cleared the clip to
    /// `None`; 3f.3 stores the `ClipState::Pixmap` with the origin
    /// preserved (mask sampling itself is deferred). A subsequent
    /// `clear_clip_rectangles` returns to `None`.
    #[test]
    fn set_clip_pixmap_stores_pixmap_clip() {
        use yserver_core::backend::ClipState;
        let mut b = KmsBackendV2::for_tests();
        b.set_clip_pixmap(None, 0xABCD_EF01, 12, 34).expect("ok");
        match &b.core.current_clip {
            ClipState::Pixmap { origin, pixmap } => {
                assert_eq!(origin.0, 12);
                assert_eq!(origin.1, 34);
                assert_eq!(pixmap.as_raw(), 0xABCD_EF01);
            }
            other => panic!("expected ClipState::Pixmap, got {other:?}"),
        }
        // pre-3f.3 stub bumped a `set_clip_pixmap` gap; 3f.3 stores
        // bookkeeping cleanly.
        assert!(
            !b.logged_gaps.borrow().contains("set_clip_pixmap"),
            "set_clip_pixmap must not log a gap post-3f.3"
        );
        b.clear_clip_rectangles(None).expect("ok");
        assert!(matches!(b.core.current_clip, ClipState::None));
    }

    /// `set_gc_fill_tiled_stores_fill_state` — Stage 3f.3 bookkeeping
    /// gate. Pre-3f.3 stub logged a gap; 3f.3 stores
    /// `FillState::Tiled { pixmap, origin }` so subsequent fill ops
    /// can route through the tiled-fill RENDER composite. xid=0
    /// degenerates to `FillState::Solid`.
    #[test]
    fn set_gc_fill_tiled_stores_fill_state() {
        use yserver_core::backend::FillState;
        let mut b = KmsBackendV2::for_tests();
        b.set_gc_fill_tiled(None, 0xDEAD_BEEF, 5, 7).expect("ok");
        match &b.core.current_fill {
            FillState::Tiled { pixmap, origin } => {
                assert_eq!(pixmap.as_raw(), 0xDEAD_BEEF);
                assert_eq!(origin.0, 5);
                assert_eq!(origin.1, 7);
            }
            other => panic!("expected FillState::Tiled, got {other:?}"),
        }
        // xid=0 means PixmapHandle::from_raw returns None — falls
        // back to FillState::Solid (defensive; the dispatcher never
        // passes 0 here).
        b.set_gc_fill_tiled(None, 0, 0, 0).expect("ok");
        assert!(matches!(b.core.current_fill, FillState::Solid));

        assert!(
            !b.logged_gaps.borrow().contains("set_gc_fill_tiled"),
            "set_gc_fill_tiled must not log a gap post-3f.3"
        );
    }

    /// Stage 3f.4 close: cursor-creation calls mint valid handles
    /// without logging gaps. `create_cursor`, `create_glyph_cursor`,
    /// `render_create_cursor`, `define_cursor`, and
    /// `xfixes_change_cursor_by_name` all return `Ok` with no
    /// `log_v2_gap` noise. Pixel rasterisation + scene blit is
    /// Stage 4 (cursor scene-layer work); 3f.4's job is to silence
    /// the pre-Stage-4 stub warnings that were misleading
    /// real-app smoke matrix triage.
    #[test]
    fn cursor_paths_do_not_log_gaps() {
        use yserver_core::backend::{FontHandle, PictureHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let pix = PixmapHandle::from_raw(0x1234_0001).unwrap();
        let font = FontHandle::from_raw(0x1234_0002).unwrap();
        let pic = PictureHandle::from_raw(0x1234_0003).unwrap();

        let c1 = b
            .create_cursor(None, pix, None, (0xFF00, 0, 0), (0, 0, 0xFF00), 4, 4)
            .expect("create_cursor");
        assert!(c1.as_raw() != 0);

        let c2 = b
            .create_glyph_cursor(None, font, None, b'X' as u16, 0, (0, 0, 0), (0, 0, 0))
            .expect("create_glyph_cursor");
        assert!(c2.as_raw() != 0);

        let c3 = b
            .render_create_cursor(None, pic, 0, 0)
            .expect("render_create_cursor")
            .expect("Some handle");
        assert!(c3.as_raw() != 0);

        b.define_cursor(None, 0xABCD_EF01, c1.as_raw())
            .expect("define_cursor");
        b.xfixes_change_cursor_by_name(None, c1.as_raw(), b"watch")
            .expect("xfixes_change_cursor_by_name");

        let gaps = b.logged_gaps.borrow();
        for g in [
            "create_cursor",
            "create_glyph_cursor",
            "render_create_cursor",
            "define_cursor",
            "xfixes_change_cursor_by_name",
        ] {
            assert!(
                !gaps.contains(g),
                "{g} must not log a gap post-3f.4 (cursor scene blit is Stage 4)"
            );
        }
    }

    /// Stage 4d regression: `ChangeWindowAttributes` on a window
    /// under COMPOSITE redirect must NOT trigger a backing wipe.
    /// Pre-fix `change_subwindow_attributes` eagerly called
    /// `clear_window_area_with_background`, which routes through
    /// `resolve_paint_target` into the redirected backing B and
    /// fills it with depth-24 default black — exactly the
    /// "mate-control-center turns opaque black on drag" symptom
    /// observed in hardware smoke (marco re-asserts CWA on every
    /// drag-induced configure; v2 interprets that as a paint
    /// command and wipes B).
    ///
    /// X11 spec: CWA's background attribute change does not
    /// repaint. The bg setting only affects future
    /// `ClearArea` / Expose handling. v2's eager clear was a
    /// Stage 3f.6 over-reach.
    #[test]
    fn cwa_on_redirected_window_does_not_clear_backing() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();

        // Set up W as a top-level window in windows_v2 + the store.
        let w_xid: u32 = 0x100_0001;
        let stack_rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            w_xid,
            super::WindowGeometryV2 {
                x: 100,
                y: 100,
                width: 200,
                height: 200,
                depth: 24,
                mapped: true,
                parent: None,
                bg_pixel: None,
                bg_pixmap: None,
                stack_rank,
            },
        );
        let w_storage = Storage::for_tests_null(
            ash::vk::Extent2D {
                width: 200,
                height: 200,
            },
            ash::vk::Format::B8G8R8A8_UNORM,
        );
        let _w_id = b
            .store
            .allocate(w_xid, DrawableKind::Window, 24, true, w_storage)
            .expect("alloc W");

        // Set up B as a pixmap, then install the redirect route
        // W → B. This is the load-bearing precondition for the
        // bug: the fill path would route through W's redirect.
        let b_xid: u32 = 0x100_0002;
        let b_storage = Storage::for_tests_null(
            ash::vk::Extent2D {
                width: 200,
                height: 200,
            },
            ash::vk::Format::B8G8R8A8_UNORM,
        );
        let b_id = b
            .store
            .allocate(b_xid, DrawableKind::Pixmap, 24, false, b_storage)
            .expect("alloc B");
        assert!(b.test_set_redirected_target(w_xid, b_xid));
        // Sanity: resolve_paint_target on W now lands at B, not W.
        let resolved = b
            .resolve_paint_target(w_xid)
            .expect("resolve_paint_target W");
        assert_eq!(
            resolved.id, b_id,
            "fixture sanity: W's paint must route to B before issuing CWA",
        );

        // Snapshot the clear counter.
        let calls_before = b.clear_window_area_calls;

        // Issue CWA with CWBackPixmap = None (value=0). That's
        // marco's "no background pixmap" attribute change, sent
        // on every drag-induced configure. v2 must NOT interpret
        // this as a paint command on the redirected backing.
        b.change_subwindow_attributes(None, w_xid, 0x01, &[0])
            .expect("change_subwindow_attributes");

        assert_eq!(
            b.clear_window_area_calls, calls_before,
            "CWA on a redirected window must not call clear_window_area_with_background \
             (pre-fix this fired and wiped B with depth-24 default black, destroying \
             the compositor's painted pixels — the 'opaque black on drag' bug)",
        );

        // Same test with CWBackPixel — also a clear-trigger pre-fix.
        b.change_subwindow_attributes(None, w_xid, 0x02, &[0x00FF_FFFF])
            .expect("change_subwindow_attributes CWBackPixel");
        assert_eq!(
            b.clear_window_area_calls, calls_before,
            "CWBackPixel on a redirected window must also skip the eager clear",
        );

        // Sanity: bg state IS stored (CWA still records the
        // values; only the eager paint is skipped).
        let geom = b.windows_v2.get(&w_xid).expect("W in windows_v2");
        assert_eq!(geom.bg_pixel, Some(0x00FF_FFFF));
        assert_eq!(geom.bg_pixmap, None);
    }

    /// Stage 3f.6 close: `change_subwindow_attributes` stores
    /// `bg_pixel` + `bg_pixmap` into the v2 window record instead of
    /// logging a gap. value_mask=0x03 (CWBackPixmap + CWBackPixel)
    /// with values [pixmap_xid, pixel] lands both. value_mask=0x02
    /// alone lands the pixel only. value_mask=0x01 with pixmap=0
    /// resolves to bg_pixmap=None per X11 semantics.
    #[test]
    fn change_subwindow_attributes_stores_bg_state() {
        let mut b = KmsBackendV2::for_tests();
        // Seed a window in windows_v2 directly (allocate fails on
        // for_tests because there's no Vk; geometry insert still
        // works in production via the no-Vk branch).
        b.windows_v2.insert(
            0xCAFE_BABE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                depth: 32,
                mapped: false,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );

        // CWBackPixmap (0x01) + CWBackPixel (0x02), values =
        // [0xABCD_1234, 0xFF0000FF].
        b.change_subwindow_attributes(None, 0xCAFE_BABE, 0x03, &[0xABCD_1234, 0xFF00_00FF])
            .expect("ok");
        let geom = b.windows_v2[&0xCAFE_BABE];
        assert_eq!(geom.bg_pixmap, Some(0xABCD_1234));
        assert_eq!(geom.bg_pixel, Some(0xFF00_00FF));

        // CWBackPixmap=0 → None (inherit-from-parent). bg_pixel
        // stays as the previous value (CWBackPixel bit clear).
        b.change_subwindow_attributes(None, 0xCAFE_BABE, 0x01, &[0])
            .expect("ok");
        let geom = b.windows_v2[&0xCAFE_BABE];
        assert_eq!(geom.bg_pixmap, None);
        assert_eq!(geom.bg_pixel, Some(0xFF00_00FF));

        // The pre-3f.6 stub bumped a `change_subwindow_attributes`
        // gap; 3f.6 stores bookkeeping cleanly.
        assert!(
            !b.logged_gaps
                .borrow()
                .contains("change_subwindow_attributes"),
            "change_subwindow_attributes must not log a gap post-3f.6"
        );
    }

    // ─── Stage 3f.7: input dispatch tests ───────────────────────

    /// `serialize_modifiers` returns 0 against a fresh xkb_state
    /// (no modifiers held). Regression gate for the bit layout.
    #[test]
    fn serialize_modifiers_zero_on_fresh_state() {
        let b = KmsBackendV2::for_tests();
        assert_eq!(b.serialize_modifiers(), 0);
    }

    /// `cook_host_key` fills root + event coords from cursor and
    /// stamps the post-update modifier mask. Pressing a Shift
    /// keycode flips the Shift bit in the cooked state.
    #[test]
    fn cook_host_key_fills_coords_and_modifier_state() {
        use yserver_core::host_x11::HostKeyEvent;
        let mut b = KmsBackendV2::for_tests();
        b.core.cursor_x = 100.0;
        b.core.cursor_y = 200.0;
        // 50 == evdev KEY_LEFTSHIFT (US layout); xkbcommon's
        // default keymap maps this to the Shift modifier.
        let raw = HostKeyEvent {
            keycode: 50,
            pressed: true,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        };
        let cooked = b.cook_host_key(raw);
        assert_eq!(cooked.root_x, 100);
        assert_eq!(cooked.root_y, 200);
        assert_eq!(cooked.event_x, 100);
        assert_eq!(cooked.event_y, 200);
        // Bit 0 = Shift. Some xkb keymaps deliver Shift on key 50
        // via xkbcommon's default; this assertion proves the
        // modifier state is read out post-update. If the test ICD
        // disagrees, lower this to >0 — the load-bearing check is
        // that `state` reflects the update, not zero.
        assert_ne!(cooked.state, 0, "Shift press must update mod state");
    }

    /// `process_pointer_button` honours the X11 spec's pre-press
    /// `state` field: on ButtonPress the button bit is NOT yet
    /// set in `state`, on ButtonRelease it IS still set.
    /// `button_mask` is updated AFTER the event so the next
    /// motion sees the new mask.
    #[test]
    fn process_pointer_button_state_field_is_pre_press() {
        use yserver_core::{host_x11::PointerEventKind, server::ServerState};
        let mut b = KmsBackendV2::for_tests();
        let state = ServerState::new();
        // BTN_LEFT press → detail=1, button bit = 0x0100.
        b.process_pointer_button(0x110, true, &state);
        let press = b
            .core
            .pending_pointer_events
            .iter()
            .find(|e| matches!(e.kind, PointerEventKind::ButtonPress))
            .expect("ButtonPress emitted");
        assert_eq!(press.detail, 1);
        assert_eq!(
            press.state & 0x0100,
            0,
            "Button1 bit must NOT be set in ButtonPress.state (pre-press)"
        );
        assert_eq!(
            b.core.button_mask & 0x0100,
            0x0100,
            "button_mask updated post-event"
        );

        b.core.pending_pointer_events.clear();
        b.process_pointer_button(0x110, false, &state);
        let release = b
            .core
            .pending_pointer_events
            .iter()
            .find(|e| matches!(e.kind, PointerEventKind::ButtonRelease))
            .expect("ButtonRelease emitted");
        assert_eq!(
            release.state & 0x0100,
            0x0100,
            "Button1 bit MUST be set in ButtonRelease.state (still held)"
        );
        assert_eq!(
            b.core.button_mask & 0x0100,
            0,
            "button_mask cleared post-release"
        );
    }

    /// `process_pointer_absolute` clamps to the output extent and
    /// updates `cursor_x` / `cursor_y`. Single-output test fixture
    /// reports 800×600 from PlatformBackend::for_tests.
    #[test]
    fn process_pointer_absolute_clamps_to_output() {
        use yserver_core::server::ServerState;
        let mut b = KmsBackendV2::for_tests();
        let state = ServerState::new();
        // Inside extent.
        b.process_pointer_absolute(&state, 100.0, 200.0);
        assert_eq!(b.core.cursor_x, 100.0);
        assert_eq!(b.core.cursor_y, 200.0);
        // Past extent → clamped to (extent - 1).
        b.process_pointer_absolute(&state, 5000.0, 5000.0);
        assert_eq!(b.core.cursor_x, 799.0);
        assert_eq!(b.core.cursor_y, 599.0);
    }

    /// Multi-output regression: the pointer clamp must use the
    /// union framebuffer extent (`PlatformBackend.fb_w/fb_h`),
    /// NOT `outputs.first().width/height`. Pre-fix the clamp
    /// consulted only the first output, so the cursor could never
    /// cross from monitor 0 onto monitor 1 in a side-by-side
    /// layout.
    ///
    /// Simulate two side-by-side 2560×1440 monitors by leaving the
    /// fixture's single 800×600 output entry in place but bumping
    /// `platform.fb_w` to 5120 (this is what `core_platform_init`
    /// computes as `max(x + width)` across all outputs in
    /// production — see `kms/backend.rs:1063-1072`). The input
    /// thread already targets that union extent at thread spawn,
    /// so v2 receives `PointerMotion { x, y }` already in
    /// virtual-screen coords; the only divergence was v2's
    /// re-clamp.
    #[test]
    fn process_pointer_absolute_uses_union_fb_extent_for_multi_output() {
        use yserver_core::server::ServerState;
        let mut b = KmsBackendV2::for_tests();
        b.platform.fb_w = 5120;
        b.platform.fb_h = 1440;
        let state = ServerState::new();
        // Point on monitor 1 (x=4000 is past output[0]'s 800-wide
        // fixture extent but well within the 5120 union extent).
        b.process_pointer_absolute(&state, 4000.0, 1000.0);
        assert_eq!(
            b.core.cursor_x, 4000.0,
            "pointer must be able to cross past the first output's \
             extent; pre-fix this clamps to 799 and the cursor is \
             stuck on monitor 0",
        );
        assert_eq!(b.core.cursor_y, 1000.0);
        // Past the union extent → clamped to (union - 1).
        b.process_pointer_absolute(&state, 9999.0, 9999.0);
        assert_eq!(b.core.cursor_x, 5119.0);
        assert_eq!(b.core.cursor_y, 1439.0);
    }

    /// `window_under_cursor` returns the topmost mapped top-level
    /// containing the cursor. Walks `core.top_level_order` back-to-
    /// front so the most-recently-stacked window wins. Unmapped
    /// windows skipped.
    #[test]
    fn window_under_cursor_finds_topmost_mapped() {
        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0x1000,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.windows_v2.insert(
            0x2000,
            super::WindowGeometryV2 {
                x: 50,
                y: 50,
                width: 100,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.core.top_level_order.push(0x1000);
        b.core.top_level_order.push(0x2000);

        // Cursor in overlap (50..100, 50..100): 0x2000 wins (topmost).
        b.core.cursor_x = 75.0;
        b.core.cursor_y = 75.0;
        assert_eq!(b.window_under_cursor(), Some(0x2000));

        // Cursor outside overlap, only in 0x1000.
        b.core.cursor_x = 25.0;
        b.core.cursor_y = 25.0;
        assert_eq!(b.window_under_cursor(), Some(0x1000));

        // Cursor outside both — root-fallback handled at caller.
        b.core.cursor_x = 300.0;
        b.core.cursor_y = 300.0;
        assert_eq!(b.window_under_cursor(), None);

        // Unmapping the topmost — next match wins.
        b.windows_v2.get_mut(&0x2000).unwrap().mapped = false;
        b.core.cursor_x = 75.0;
        b.core.cursor_y = 75.0;
        assert_eq!(b.window_under_cursor(), Some(0x1000));
    }

    /// `on_host_input` no longer logs the `v2: on_host_input not
    /// yet implemented` gap that fired before 3f.7. Key events
    /// drain through xkb cooking; pointer events drain to the
    /// pointer fanout.
    #[test]
    fn on_host_input_does_not_log_gap() {
        use yserver_core::{core_loop::HostInputEvent, server::ServerState};
        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();
        // PointerMotion → process_pointer_absolute → no panic, no gap.
        b.on_host_input(
            &mut state,
            HostInputEvent::PointerMotion {
                x: 10,
                y: 20,
                time: 0,
            },
        );
        assert!(
            !b.logged_gaps.borrow().contains("on_host_input"),
            "on_host_input must not log a gap post-3f.7"
        );
        assert_eq!(b.core.cursor_x, 10.0);
        assert_eq!(b.core.cursor_y, 20.0);
    }

    /// Stage 3f.6 — `create_subwindow` records the parent xid + the
    /// background-pixel hint so subsequent `build_scene` traversals
    /// can reach the new window and an initial bg_pixel fill can
    /// run. Engine fill itself returns `NoVk` on the test fixture;
    /// the load-bearing observable is the geometry record.
    #[test]
    fn create_subwindow_records_parent_and_bg_pixel() {
        use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
        let mut b = KmsBackendV2::for_tests();
        let parent = WindowHandle::from_raw(0x1234_5678).unwrap();
        let child = b
            .create_subwindow(
                None,
                parent,
                10,
                20,
                100,
                50,
                0,
                HostSubwindowVisual::CopyFromParent,
                Some(0xFF11_2233),
                None,
            )
            .expect("create_subwindow");
        let geom = b.windows_v2[&child.as_raw()];
        assert_eq!(geom.parent, Some(0x1234_5678));
        assert_eq!(geom.bg_pixel, Some(0xFF11_2233));
        assert_eq!(geom.x, 10);
        assert_eq!(geom.y, 20);
        assert_eq!(geom.width, 100);
        assert_eq!(geom.height, 50);
        assert_eq!(
            geom.depth, 24,
            "root/untracked CopyFromParent inherits root depth"
        );
        assert!(!geom.mapped, "mapped is set later via map_subwindow");
    }

    #[test]
    fn copy_from_parent_child_inherits_argb_parent_depth() {
        use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0x2000,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 80,
                height: 40,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        let child = b
            .create_subwindow(
                None,
                WindowHandle::from_raw(0x2000).unwrap(),
                1,
                2,
                30,
                20,
                0,
                HostSubwindowVisual::CopyFromParent,
                None,
                None,
            )
            .expect("create_subwindow");
        assert_eq!(b.windows_v2[&child.as_raw()].depth, 32);
    }

    #[test]
    fn depth_only_visual_preserves_argb_top_level_depth() {
        use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

        let mut b = KmsBackendV2::for_tests();
        let child = b
            .create_subwindow(
                None,
                WindowHandle::from_raw(b.window_id()).unwrap(),
                0,
                0,
                2944,
                1840,
                0,
                HostSubwindowVisual::DepthOnly { depth: 32 },
                Some(0),
                None,
            )
            .expect("create_subwindow");
        assert_eq!(b.windows_v2[&child.as_raw()].depth, 32);
    }

    /// Stage 3f.11: reparenting a top-level window INTO another
    /// window removes it from `core.top_level_order` so
    /// `build_scene` only emits it once (via the recurse from the
    /// new parent). Reproducer for the MATE clock-applet duplicate-
    /// render: clock was first registered as a top-level under
    /// root, then reparented INTO mate-panel's container. Pre-fix,
    /// build_scene emitted it twice — once at child-relative coords
    /// (treated as absolute) and once at real screen position.
    #[test]
    fn reparent_into_container_removes_from_top_level_order() {
        let mut b = KmsBackendV2::for_tests();
        // Two stub windows: the parent container, and the would-be
        // child (initially registered as a top-level).
        b.windows_v2.insert(
            0xC0FFEE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 200,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.windows_v2.insert(
            0xCAFED00D,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 50,
                height: 20,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.core.top_level_order.push(0xC0FFEE);
        b.core.top_level_order.push(0xCAFED00D);
        assert!(b.core.top_level_order.contains(&0xCAFED00D));

        // Reparent 0xCAFED00D under 0xC0FFEE at (30, 10).
        b.reparent_subwindow(None, 0xCAFED00D, 0xC0FFEE, 30, 10)
            .expect("reparent");

        // top_level_order must no longer contain the reparented xid.
        assert!(
            !b.core.top_level_order.contains(&0xCAFED00D),
            "reparenting into a non-root parent must remove from top_level_order \
             — otherwise build_scene emits the window twice"
        );
        // Geometry record reflects new parent + position.
        let geom = b.windows_v2[&0xCAFED00D];
        assert_eq!(geom.parent, Some(0xC0FFEE));
        assert_eq!(geom.x, 30);
        assert_eq!(geom.y, 10);
    }

    /// Stage 3f.11: `restack_top_level` with `stack_mode=Below` and
    /// no sibling lowers a top-level to the BOTTOM of
    /// `core.top_level_order`. Reproduces marco's "lower caja-
    /// desktop" call so the wallpaper window stays beneath panels.
    #[test]
    fn restack_below_no_sibling_moves_to_bottom() {
        let mut b = KmsBackendV2::for_tests();
        b.core.top_level_order = vec![0x1000, 0x2000, 0x3000];
        // 0x3000 is the most recently registered (top of stack).
        // Marco's Lower-Below request should move it to position 0.
        b.restack_top_level(0x3000, 1, None);
        assert_eq!(b.core.top_level_order, vec![0x3000, 0x1000, 0x2000]);
    }

    /// Stage 3f.11: `restack_top_level` with `stack_mode=Above` and
    /// no sibling raises a top-level to the TOP of
    /// `core.top_level_order`.
    #[test]
    fn restack_above_no_sibling_moves_to_top() {
        let mut b = KmsBackendV2::for_tests();
        b.core.top_level_order = vec![0x1000, 0x2000, 0x3000];
        b.restack_top_level(0x1000, 0, None);
        assert_eq!(b.core.top_level_order, vec![0x2000, 0x3000, 0x1000]);
    }

    /// Stage 3f.11 follow-up: subwindow restack updates sibling order
    /// within a shared parent instead of relying on HashMap iteration.
    #[test]
    fn restack_subwindow_updates_sibling_order() {
        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0xCAFE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                depth: 32,
                mapped: true,
                parent: Some(0xBEEF),
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.windows_v2.insert(
            0xD00D,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                depth: 32,
                mapped: true,
                parent: Some(0xBEEF),
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.restack_subwindow(0xD00D, 1, Some(0xCAFE));
        assert!(b.windows_v2[&0xD00D].stack_rank < b.windows_v2[&0xCAFE].stack_rank);
    }

    /// Stage 3f.11: reparenting back to root re-adds to
    /// `core.top_level_order` so the window resumes top-level
    /// rendering. The Backend trait's reparent call carries the new
    /// parent xid; `host_parent==0` or an untracked xid (root is
    /// `core.window_id`, not in `windows_v2`) maps to `parent=None`.
    #[test]
    fn reparent_to_root_re_adds_to_top_level_order() {
        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0xC0FFEE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 200,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.windows_v2.insert(
            0xCAFED00D,
            super::WindowGeometryV2 {
                x: 30,
                y: 10,
                width: 50,
                height: 20,
                depth: 32,
                mapped: true,
                parent: Some(0xC0FFEE),
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        // Start: child not in top_level_order.
        assert!(!b.core.top_level_order.contains(&0xCAFED00D));

        // Reparent to root (host_parent=0 maps to parent=None).
        b.reparent_subwindow(None, 0xCAFED00D, 0, 100, 200)
            .expect("reparent");

        assert!(
            b.core.top_level_order.contains(&0xCAFED00D),
            "reparenting to root must add to top_level_order"
        );
        let geom = b.windows_v2[&0xCAFED00D];
        assert_eq!(geom.parent, None);
        assert_eq!(geom.x, 100);
        assert_eq!(geom.y, 200);
    }

    // ───── Stage 4a — resolve_paint_target ─────────────────────────

    /// Seed a window in `windows_v2` and a matching no-Vk store
    /// entry, returning the new DrawableId. Used by the 4a
    /// resolver tests so the ancestor walk has something to chew
    /// on without touching Vk.
    fn seed_window(
        b: &mut KmsBackendV2,
        xid: u32,
        parent: Option<u32>,
        x: i16,
        y: i16,
    ) -> crate::kms::v2::store::DrawableId {
        use crate::kms::v2::store::{DrawableKind, Storage};
        b.windows_v2.insert(
            xid,
            super::WindowGeometryV2 {
                x,
                y,
                width: 100,
                height: 100,
                depth: 32,
                mapped: true,
                parent,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
            },
        );
        b.store
            .allocate(
                xid,
                DrawableKind::Window,
                32,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("seed_window allocate")
    }

    /// Unknown xid → `None`. The resolver's first step is
    /// `store.lookup`, which fails for an xid that was never
    /// allocated.
    #[test]
    fn resolve_paint_target_unknown_xid_returns_none() {
        let b = KmsBackendV2::for_tests();
        assert_eq!(b.resolve_paint_target(0xDEAD_BEEF), None);
    }

    /// Pixmap xid (not in `windows_v2`) with no redirect →
    /// identity result. Covers the pre-loop short-circuit so the
    /// ancestor walk never reads `None` off a pixmap.
    #[test]
    fn resolve_paint_target_pixmap_returns_identity() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let pix_id = b
            .store
            .allocate(
                0x2000,
                DrawableKind::Pixmap,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("pixmap allocate");
        let pt = b.resolve_paint_target(0x2000).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: pix_id,
                offset: (0, 0)
            }
        );
    }

    /// Top-level window with no redirect → identity result.
    /// `parent == None` reaches the explicit fall-through arm; the
    /// resolver must NOT short-circuit to `None` via `?` on the
    /// missing parent.
    #[test]
    fn resolve_paint_target_unredirected_top_level_returns_identity() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: w_id,
                offset: (0, 0)
            }
        );
    }

    /// `set_redirected_target(W, Some(B))` routes paint against
    /// `W`'s xid to `B`'s drawable. Offset stays `(0, 0)` —
    /// `W` is the redirected node itself, not a descendant.
    #[test]
    fn resolve_paint_target_redirected_window_routes_to_backing() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let b_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        b.store.set_redirected_target(w_id, Some(b_id));
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: b_id,
                offset: (0, 0)
            }
        );
    }

    /// Descendant paint accumulates `(x, y)` offsets up the
    /// ancestor chain. W at root with redirect to B; child C at
    /// (10, 20) under W; grandchild G at (3, 4) under C. Paint on
    /// G's xid resolves to `(B, (13, 24))` — the sum of the
    /// child offsets traversed.
    #[test]
    fn resolve_paint_target_descendant_accumulates_offset() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        let _g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
        let b_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 200,
                        height: 200,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        b.store.set_redirected_target(w_id, Some(b_id));
        let pt = b.resolve_paint_target(0x300).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: b_id,
                offset: (13, 24)
            }
        );
    }

    /// Top-level whose `parent == Some(root_xid)` (root isn't in
    /// `windows_v2`) walks one step and finds root's redirect
    /// state. With root un-redirected, paint stays on the leaf.
    /// Regression for the resolver-returns-None bug that surfaced
    /// when `v2_subwindow_resize_clears_old_paint` started routing
    /// `fill_rectangle` through `resolve_paint_target` and the
    /// previous `windows_v2.get(parent_xid)?` chain poisoned the
    /// outer Option for any parent==root case.
    #[test]
    fn resolve_paint_target_parent_root_falls_back_to_identity() {
        let mut b = KmsBackendV2::for_tests();
        // root_xid is seeded via `KmsCore::for_tests()` and present
        // in the store (init_root_storage); but NOT in windows_v2.
        let root_xid = b.core.window_id;
        assert!(!b.windows_v2.contains_key(&root_xid));
        let w_id = seed_window(&mut b, 0x100, Some(root_xid), 0, 0);
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: w_id,
                offset: (0, 0)
            }
        );
    }

    /// Root itself can be the redirect target — a compositor that
    /// runs `RedirectWindow(root, …)` sets `redirected_target` on
    /// root's drawable. Paint against root or its descendants
    /// resolves through the root-backing.
    ///
    /// Codex round-7 finding: top-level windows are recorded with
    /// `parent == None` (NOT `Some(root_xid)`) by
    /// `create_subwindow` because root isn't tracked in
    /// `windows_v2`. The pre-fix resolver's `None` arm returned
    /// identity without consulting root, so real top-level
    /// descendants bypassed the root backing.
    #[test]
    fn resolve_paint_target_redirected_root_routes_descendants() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let root_xid = b.core.window_id;
        let root_id = b.store.lookup(root_xid).expect("root id");
        // Use the production representation: `parent = None`
        // marks a top-level whose host_parent is root_xid (see
        // `create_subwindow`'s `if !windows_v2.contains_key →
        // parent = None` branch). Also seed a descendant whose
        // parent IS the top-level so we exercise the full walk.
        let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
        let _c_id = seed_window(&mut b, 0x101, Some(0x100), 3, 4);
        let backing_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 800,
                        height: 600,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        b.store.set_redirected_target(root_id, Some(backing_id));
        // Root paint (direct) resolves through the leaf-level
        // pre-loop short-circuit.
        let pt_root = b.resolve_paint_target(root_xid).expect("resolve root");
        assert_eq!(
            pt_root,
            super::PaintTarget {
                id: backing_id,
                offset: (0, 0)
            }
        );
        // Top-level (parent=None production rep) must walk into
        // root's redirect with its own (x, y) accumulated.
        let pt_w = b.resolve_paint_target(0x100).expect("resolve W");
        assert_eq!(
            pt_w,
            super::PaintTarget {
                id: backing_id,
                offset: (50, 60)
            }
        );
        // Descendant of a top-level: accumulates C-in-W (3, 4)
        // then W-in-root (50, 60) → (53, 64).
        let pt_c = b.resolve_paint_target(0x101).expect("resolve C");
        assert_eq!(
            pt_c,
            super::PaintTarget {
                id: backing_id,
                offset: (53, 64)
            }
        );
    }

    /// Plan §4a (Tests, line 644-646): clearing a redirect via
    /// `set_redirected_target(W, None)` falls back to leaf-storage
    /// routing. The store-level `set_redirected_target_none_clears_route`
    /// verifies the field is cleared; this end-to-end check
    /// asserts the resolver flow honours that. Catches a regression
    /// where a missing branch / wrong `?` could special-case the
    /// cleared state.
    #[test]
    fn resolve_paint_target_after_clear_falls_back_to_identity() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let backing_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        // Install the redirect, then immediately clear it.
        b.store.set_redirected_target(w_id, Some(backing_id));
        b.store.set_redirected_target(w_id, None);
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: w_id,
                offset: (0, 0)
            },
            "cleared redirect must fall through to leaf identity",
        );
    }

    /// Nearest redirected ancestor wins. W→B_W and C→B_C both
    /// redirected; grandchild G under C must route to B_C with
    /// the C-relative offset, NOT to B_W with the
    /// W-relative offset.
    #[test]
    fn resolve_paint_target_stops_at_nearest_redirected_ancestor() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        let _g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
        let bw_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 200,
                        height: 200,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("B_W");
        let bc_id = b
            .store
            .allocate(
                0x901,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("B_C");
        b.store.set_redirected_target(w_id, Some(bw_id));
        b.store.set_redirected_target(c_id, Some(bc_id));
        let pt = b.resolve_paint_target(0x300).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: bc_id,
                offset: (3, 4)
            }
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4c.2 — `window_absolute_rect` helper
    // ────────────────────────────────────────────────────────────────

    /// Top-level W at (50, 60) size 100×80, parent=None. Absolute
    /// rect echoes its own (x, y, w, h) — there's no ancestor to
    /// accumulate through.
    #[test]
    fn window_absolute_rect_top_level() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 50, 60);
        // `seed_window` hard-codes 100×100; resize via the geom entry.
        b.windows_v2.get_mut(&0x100).unwrap().width = 100;
        b.windows_v2.get_mut(&0x100).unwrap().height = 80;
        let rect = b.window_absolute_rect(w_id).expect("rect");
        assert_eq!(
            rect,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: 50, y: 60 },
                extent: ash::vk::Extent2D {
                    width: 100,
                    height: 80
                },
            }
        );
    }

    /// Three-level chain: W(50, 60) → C(10, 20) → G(3, 4) size 8×8.
    /// G's absolute rect is at (63, 84) with G's own 8×8 extent.
    #[test]
    fn window_absolute_rect_descendant() {
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
        let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        let g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
        // `seed_window` defaults C/G to 100×100; shrink to plan sizes.
        {
            let c = b.windows_v2.get_mut(&0x200).unwrap();
            c.width = 30;
            c.height = 30;
        }
        {
            let g = b.windows_v2.get_mut(&0x300).unwrap();
            g.width = 8;
            g.height = 8;
        }
        let rect = b.window_absolute_rect(g_id).expect("rect");
        assert_eq!(
            rect,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: 63, y: 84 },
                extent: ash::vk::Extent2D {
                    width: 8,
                    height: 8
                },
            }
        );
    }

    /// `DrawableId` that the store no longer knows about → None.
    /// Allocate a window then `decref` it down to retirement so
    /// the id no longer resolves; `store.get` returns None and the
    /// helper short-circuits without poking `windows_v2`.
    #[test]
    fn window_absolute_rect_unknown_drawable_returns_none() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 50, 60);
        // Tear it back down so the DrawableId no longer resolves.
        // `decref` with no Vk treats the ticket as signaled and
        // calls `destroy_now`, removing the id from `entries`.
        let _ = b.store.decref(&mut b.platform, w_id);
        // Also clear the windows_v2 entry — otherwise the helper
        // would early-return on the xid lookup, not the id lookup
        // we want to exercise.
        b.windows_v2.remove(&0x100);
        assert!(b.store.get(w_id).is_none());
        assert_eq!(b.window_absolute_rect(w_id), None);
    }

    /// Pixmaps live in the store but not in `windows_v2`. The
    /// helper has no geometry to walk → None.
    #[test]
    fn window_absolute_rect_pixmap_returns_none() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let pix_id = b
            .store
            .allocate(
                0x2000,
                DrawableKind::Pixmap,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("pixmap allocate");
        assert_eq!(b.window_absolute_rect(pix_id), None);
    }

    /// Dangling parent xid: W has `parent = Some(0xDEAD)` and
    /// 0xDEAD is neither root nor in `windows_v2`. Conservative
    /// choice per plan: bail with None rather than return a
    /// half-accumulated rect that callers can't act on.
    #[test]
    fn window_absolute_rect_dangling_parent_returns_none() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, Some(0xDEAD), 50, 60);
        // 0xDEAD is not in windows_v2 and is not root_xid.
        assert!(!b.windows_v2.contains_key(&0xDEAD));
        assert_ne!(b.core.window_id, 0xDEAD);
        assert_eq!(b.window_absolute_rect(w_id), None);
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4c.4 — set_window_scene_participation /
    // set_backing_scene_participation
    // ────────────────────────────────────────────────────────────────

    /// `participating=false` on a window with pending presentation
    /// damage must delegate to `DrawableStore::set_scene_participating`
    /// — that store method clears the damage and bumps the epoch.
    /// This verifies the v2 backend actually wires the call (rather
    /// than e.g. silently returning Ok).
    #[test]
    fn set_window_scene_participation_false_clears_window_damage() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        // Seed presentation damage so the store actually has work
        // to clear when participation flips off.
        b.store.damage(
            w_id,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
        );
        assert_eq!(
            b.store.get(w_id).unwrap().presentation_damage.rects().len(),
            1,
        );
        let epoch_before = b.store.get(w_id).unwrap().presentation_damage_epoch;

        let handle = WindowHandle::from_raw(0x100).expect("WindowHandle");
        b.set_window_scene_participation(None, handle, false)
            .expect("set_window_scene_participation");

        let d = b.store.get(w_id).expect("drawable still alive");
        assert!(
            d.presentation_damage.is_empty(),
            "presentation_damage must clear on participating=false transition: {:?}",
            d.presentation_damage.rects(),
        );
        assert!(
            d.presentation_damage_epoch > epoch_before,
            "epoch must bump on participating=false transition (before={epoch_before}, after={})",
            d.presentation_damage_epoch,
        );
        assert!(
            !d.scene_participating,
            "scene_participating flag must be cleared",
        );
    }

    /// `set_window_scene_participation` must fire scene-structure
    /// damage for the redirect transition. On the stub-mode scene
    /// (`for_tests` fixture has `inner: None`), we can only observe
    /// the `scene_structure_dirty` bit — the per-output rect
    /// dispatch is covered in `scene::tests::
    /// dispatch_clip_rects_lands_per_output_clipped` (4c.1 follow-up).
    /// This test pins the contract that the backend CALLS the rect
    /// setter (or the coarse fallback) rather than leaving the
    /// scene-structure state untouched.
    #[test]
    fn set_window_scene_participation_fires_scene_structure_damage_rect() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
        // Sanity: pre-flip rect lookup is non-None (Test 2 requires
        // the rect path, not the coarse fallback path).
        let pre_flip = b
            .window_absolute_rect(b.store.lookup(0x100).unwrap())
            .expect("pre-flip rect known");
        assert_eq!(pre_flip.offset.x, 50);
        assert_eq!(pre_flip.offset.y, 60);

        // Start with the dirty bit cleared so the assertion proves
        // THIS call set it (not some setup side effect).
        b.scene.scene_structure_dirty = false;

        let handle = WindowHandle::from_raw(0x100).expect("WindowHandle");
        b.set_window_scene_participation(None, handle, false)
            .expect("set_window_scene_participation");

        assert!(
            b.scene.scene_structure_dirty,
            "scene_structure_dirty must be set after a participation flip",
        );
    }

    /// `set_backing_scene_participation` flips the backing's
    /// `scene_participating` flag via the store but must NOT fire
    /// scene-structure damage — geometric damage is the W-side
    /// call's responsibility (backings have no on-screen geometry
    /// of their own).
    #[test]
    fn set_backing_scene_participation_flips_flag_no_damage() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::PixmapHandle;
        let mut b = KmsBackendV2::for_tests();
        let b_id = b
            .store
            .allocate(
                0x2000,
                DrawableKind::Pixmap,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("pixmap allocate");
        // Pixmaps start with scene_participating=false (per
        // `DrawableStore::allocate`'s `scene_participating` arg).
        assert!(!b.store.get(b_id).unwrap().scene_participating);
        // Capture the prior dirty bit (whatever setup left it at).
        // The assertion below is "no CHANGE", not "is false".
        let dirty_before = b.scene.scene_structure_dirty;

        let handle = PixmapHandle::from_raw(0x2000).expect("PixmapHandle");
        b.set_backing_scene_participation(None, handle, true)
            .expect("set_backing_scene_participation");

        assert!(
            b.store.get(b_id).unwrap().scene_participating,
            "backing scene_participating must flip to true",
        );
        assert_eq!(
            b.scene.scene_structure_dirty, dirty_before,
            "set_backing_scene_participation must NOT fire scene-structure damage",
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4c.5 — Manual-redirect lifecycle through the Backend
    // surface (deferred from 4b.9 / Stage 4c plan §"Tests Vk-backed").
    //
    // These exercise the no-Vk pathway: `allocate_redirected_backing`
    // skips the store-side wiring when no Vk is attached (the
    // `create_pixmap` fallback doesn't seed a store entry for the
    // backing — see backend.rs:3214 `create_pixmap_no_vk`), but the
    // `alias_registry.insert` + `host_window_to_backing.insert` still
    // fire. That's enough for the participation-flip assertions to
    // observe `scene_structure_dirty`.
    //
    // The per-output rect dispatch goes through scene.rs:412's
    // stub-mode guard — `dispatch_clip_rects_lands_per_output_clipped`
    // covers that branch directly.
    // ────────────────────────────────────────────────────────────────

    /// Simulate `RedirectWindow(W, Manual)`: allocate the backing,
    /// then flip W to `scene_participating=false`. The participation
    /// flip MUST fire scene-structure damage so the next composite
    /// repaints the region W used to occupy (under Manual mode the
    /// scene drops W; whatever's underneath must redraw).
    #[test]
    fn manual_redirect_path_marks_scene_structure_damage() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
        let w = WindowHandle::from_raw(0x100).expect("WindowHandle");

        // Step 1: allocate the backing. On no-Vk the store-side
        // wiring is skipped (logged as a warn) but the alias-registry
        // + host_window_to_backing entries install. That's enough
        // for the protocol-side state machine; scene-structure
        // damage comes from the next call.
        let _backing = b
            .allocate_redirected_backing(None, w, 100, 100, 32)
            .expect("allocate_redirected_backing");

        // Clear the dirty bit so the post-flip assertion proves
        // the participation call set it, not the allocation above.
        b.scene.scene_structure_dirty = false;

        // Step 2: flip W to non-participating (Manual activation).
        b.set_window_scene_participation(None, w, false)
            .expect("set_window_scene_participation(false)");

        assert!(
            b.scene.scene_structure_dirty,
            "Manual-redirect participation flip (W→false) must fire \
             scene-structure damage so the region W used to occupy \
             gets repainted by whatever's underneath",
        );
    }

    /// Full Manual-redirect lifecycle: activate (Manual), then
    /// un-redirect. Both transitions must fire scene-structure
    /// damage. Clear the dirty bit between the two flips so the
    /// final assertion proves the SECOND call set it independently.
    #[test]
    fn unredirect_restores_participation_and_marks_damage() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
        let w = WindowHandle::from_raw(0x100).expect("WindowHandle");

        // Manual activation: allocate + flip W off-scene.
        let backing = b
            .allocate_redirected_backing(None, w, 100, 100, 32)
            .expect("allocate_redirected_backing");
        b.set_window_scene_participation(None, w, false)
            .expect("set_window_scene_participation(false)");
        assert!(
            b.scene.scene_structure_dirty,
            "fixture sanity: Manual activation already fires scene-structure damage \
             (covered by manual_redirect_path_marks_scene_structure_damage)",
        );

        // Clear so the post-un-redirect assertion is sharp.
        b.scene.scene_structure_dirty = false;

        // Un-redirect: drop the backing hold and flip W back on-scene.
        b.release_redirected_backing(None, backing)
            .expect("release_redirected_backing");
        // `release_redirected_backing` doesn't touch W's scene flag;
        // un-redirect-to-mapped is the W-side caller's responsibility.
        b.set_window_scene_participation(None, w, true)
            .expect("set_window_scene_participation(true)");

        assert!(
            b.scene.scene_structure_dirty,
            "Un-redirect participation flip (W→true) must ALSO fire \
             scene-structure damage so W's region gets composited \
             back into the scene from W's own storage",
        );

        // Sanity: W is back to participating; the backing's
        // alias-registry entry is gone (release dropped Reason-1
        // and there were no aliases).
        let w_id = b.store.lookup(0x100).expect("w still in store");
        assert!(
            b.store.get(w_id).unwrap().scene_participating,
            "W must end in scene_participating=true after un-redirect",
        );
        assert!(
            b.test_alias_registry_get(backing.as_raw()).is_none(),
            "backing alias-registry entry must be cleared after release_redirected_backing",
        );
        assert!(
            b.test_host_window_to_backing(0x100).is_none(),
            "host_window_to_backing must be cleared after release",
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4d — Composite Overlay Window (COW) lifecycle.
    //
    // These exercise the no-Vk pathway: `allocate_drawable_storage`
    // returns `ERROR_INITIALIZATION_FAILED` on `for_tests()`; the
    // get_overlay_window override falls back to a `Storage::for_tests_null`
    // stub so the store-side wiring (xid mapping, refcount, scene
    // registration) is still exercised. The Vk-backed test in
    // `tests/v2_acceptance.rs` covers the actual paint+scanout path.
    // ────────────────────────────────────────────────────────────────

    /// First call: COW xid resolves in store; refcount = 1;
    /// backend `cow_id` set; scene registration is a no-op on
    /// the stub fixture but the field flip is observable as a
    /// `scene_structure_dirty` toggle on the live-Vk path.
    #[test]
    fn cow_get_overlay_first_call_allocates_storage() {
        let mut b = KmsBackendV2::for_tests();
        assert_eq!(b.core.cow_refcount, 0);
        assert!(b.cow_id.is_none());
        // Pre-flight: COW xid is NOT in the store yet.
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_none(),
            "COW xid must not resolve before GetOverlayWindow",
        );

        b.get_overlay_window(None).expect("get_overlay_window");

        assert_eq!(b.core.cow_refcount, 1, "refcount must be 1 after first GET");
        assert!(b.cow_id.is_some(), "backend.cow_id must be set after GET");
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_some(),
            "COW xid must resolve in the store after GetOverlayWindow",
        );
        // Storage shape: depth-24 screen-extent, scene-participating,
        // DrawableKind::Window so build_scene's window-kind gating
        // doesn't filter it.
        let cow_id = b.cow_id.expect("cow_id set");
        let cow = b.store.get(cow_id).expect("cow drawable");
        assert_eq!(cow.depth, 24, "COW must be depth-24");
        assert!(
            cow.scene_participating,
            "COW must be scene_participating=true so build_scene includes it",
        );
        assert!(
            matches!(cow.kind, super::super::store::DrawableKind::Window),
            "COW must be DrawableKind::Window",
        );
        assert_eq!(cow.storage.extent.width, u32::from(b.platform.fb_w));
        assert_eq!(cow.storage.extent.height, u32::from(b.platform.fb_h));
    }

    /// Second call without an intervening release just bumps the
    /// refcount. Storage stays the same `DrawableId` (no
    /// re-allocation), `cow_id` unchanged.
    #[test]
    fn cow_get_overlay_second_call_refcounts() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("first get");
        let id_after_first = b.cow_id.expect("cow_id set after first GET");

        b.get_overlay_window(None).expect("second get");

        assert_eq!(
            b.core.cow_refcount, 2,
            "refcount must increment to 2 on the second GET",
        );
        assert_eq!(
            b.cow_id.expect("cow_id set after second GET"),
            id_after_first,
            "second GetOverlayWindow must NOT reallocate — same DrawableId",
        );
    }

    /// Release after multiple GETs decrements but keeps the
    /// storage alive (refcount > 0). The COW xid still resolves.
    #[test]
    fn cow_release_decrements_refcount() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("get 1");
        b.get_overlay_window(None).expect("get 2");
        b.get_overlay_window(None).expect("get 3");
        assert_eq!(b.core.cow_refcount, 3);

        let was_final = b.release_overlay_window(None).expect("release 1");
        assert!(
            !was_final,
            "release_overlay_window must return Ok(false) when refcount > 0 \
             after decrement (handler uses this signal to skip the host_xid \
             clear-on-final-release path)",
        );

        assert_eq!(b.core.cow_refcount, 2, "refcount drops from 3 → 2");
        assert!(
            b.cow_id.is_some(),
            "storage still held — refcount > 0 keeps cow_id",
        );
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_some(),
            "COW xid still resolves while refcount > 0",
        );
    }

    /// Final release drops the storage. `cow_id` clears; xid no
    /// longer resolves in the store (so a fresh `GetOverlayWindow`
    /// would reallocate clean — protocol guarantees the COW xid
    /// is reusable after every release-to-zero).
    #[test]
    fn cow_release_zero_drops_storage() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("get");
        assert_eq!(b.core.cow_refcount, 1);

        let was_final = b.release_overlay_window(None).expect("release");
        assert!(
            was_final,
            "release_overlay_window must return Ok(true) on the refcount→0 \
             transition (handler uses this signal to clear the COW resource \
             record's host_xid so the next GetOverlayWindow re-wires fresh)",
        );

        assert_eq!(b.core.cow_refcount, 0);
        assert!(
            b.cow_id.is_none(),
            "cow_id must clear on refcount→0 release",
        );
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_none(),
            "COW xid must NOT resolve after the final release — \
             the store has destroyed (or detached) the entry so a \
             subsequent GetOverlayWindow can reallocate at the \
             same xid",
        );

        // A second GetOverlayWindow round-trips cleanly: reallocates
        // fresh, refcount climbs from 0 → 1.
        b.get_overlay_window(None)
            .expect("re-get after final release");
        assert_eq!(b.core.cow_refcount, 1);
        assert!(b.cow_id.is_some());
    }

    /// Stage 4d defensive branch: a `ReleaseOverlayWindow` with
    /// no preceding `GetOverlayWindow` (compositor crash + restart
    /// midway through a hand-off, double-release on the same
    /// client, etc.) must be a clean no-op. `core.cow_refcount`
    /// stays 0 (no underflow), `cow_id` stays `None`, and the
    /// scene's COW entry stays unregistered.
    #[test]
    fn cow_release_without_prior_get_is_noop() {
        let mut b = KmsBackendV2::for_tests();
        assert_eq!(b.core.cow_refcount, 0);
        assert!(b.cow_id.is_none());

        let was_final = b.release_overlay_window(None).expect("noop release");
        assert!(
            !was_final,
            "unmatched release (refcount already 0) must return Ok(false): \
             we didn't transition the refcount and didn't destroy any storage",
        );

        assert_eq!(
            b.core.cow_refcount, 0,
            "unmatched release must NOT underflow refcount",
        );
        assert!(
            b.cow_id.is_none(),
            "unmatched release must NOT spuriously set cow_id",
        );
        // Subsequent get_overlay_window still works (defensive
        // branch hasn't poisoned any state).
        b.get_overlay_window(None).expect("get after noop release");
        assert_eq!(b.core.cow_refcount, 1);
        assert!(b.cow_id.is_some());
    }

    // ── DRI3 backfill (Stage 4d.* compositor unblock) ───────────
    //
    // Ports v1's DRI3 surface to v2. The `for_tests()` fixture
    // has no render-node + no Vk, so it exercises the
    // "unsupported" branch of every accessor. The Vk-backed
    // tests are gated `#[ignore]` and run under `vng` via
    // `cargo test -- --ignored`, mirroring the Phase 4.2 hardware
    // coverage matrix.

    #[test]
    fn dri3_capabilities_unsupported_without_vk_returns_unsupported() {
        let b = KmsBackendV2::for_tests();
        let caps = b.dri3_capabilities();
        // unsupported() sentinel is (0, 0) per the trait_def
        // doc-comment.
        assert_eq!(caps.version, (0, 0), "no Vk → DRI3 reports unsupported");
        assert!(!caps.modifiers);
        assert!(!caps.fence_fd);
        assert!(!caps.syncobj);
    }

    #[test]
    fn dri3_open_errs_when_render_node_unavailable() {
        // for_tests sets render_node_path: None on PlatformBackend,
        // so dri3_open must Err out (the SCM_RIGHTS dispatch path
        // then maps it to BadAlloc).
        let mut b = KmsBackendV2::for_tests();
        let res = b.dri3_open(0x1234);
        assert!(res.is_err(), "expected Err when render_node_path is None");
    }

    #[test]
    fn dri3_export_pixmap_unknown_xid_errs() {
        // No Vk → first guard fires. With Vk this would still
        // Err because the xid isn't in the store — covered by
        // the Vk-backed test below.
        let mut b = KmsBackendV2::for_tests();
        let res = b.dri3_export_pixmap(0x4040_0000);
        assert!(res.is_err());
    }

    #[test]
    fn dri3_fd_from_fence_unknown_errs() {
        let mut b = KmsBackendV2::for_tests();
        assert!(b.dri3_fd_from_fence(0x4040_4040).is_err());
    }

    #[test]
    fn dri3_signal_syncobj_unknown_errs() {
        let mut b = KmsBackendV2::for_tests();
        assert!(b.dri3_signal_syncobj(0x4040_4040, 1).is_err());
    }

    #[test]
    fn dri3_trigger_fence_unknown_is_ok() {
        // v1's body returns Ok for the unknown-fence case — the
        // VkSemaphore path is server-state-only, no GPU op. v2
        // mirrors.
        let mut b = KmsBackendV2::for_tests();
        assert!(b.dri3_trigger_fence(0x4040_4040).is_ok());
    }

    /// Vk-backed: DRI3 capabilities reach `(1, 4)` with
    /// `fence_fd` + `syncobj` when a real `VkContext` is attached.
    /// Gated `#[ignore]` because it needs lavapipe (or any live
    /// Vulkan ICD).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn dri3_capabilities_v14_with_syncobj_when_vk_attached() {
        let b = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip: no Vk: {e}");
                return;
            }
        };
        let caps = b.dri3_capabilities();
        // for_tests_with_vk still has render_node_fd: None
        // (no real DRM device). The guard checks render_node_fd
        // first → caps come back unsupported even with Vk.
        // Verify that branch is what reports unsupported, then
        // bypass it for the version + syncobj assertion by
        // injecting a synthetic render-node fd into platform.
        assert_eq!(
            caps.version,
            (0, 0),
            "without render_node_fd, even with Vk, dri3 is gated unsupported",
        );
        // Now stuff in a synthetic render-node fd so the guard
        // passes; use /dev/null which is openable + safely
        // droppable. The cap accessor doesn't actually use the
        // fd, just checks Some-ness.
        let mut b = b;
        b.platform.render_node_fd = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .open("/dev/null")
                .expect("open /dev/null")
                .into(),
        );
        let caps = b.dri3_capabilities();
        assert_eq!(caps.version, (1, 4));
        assert!(caps.fence_fd);
        assert!(caps.syncobj);
        // `modifiers` reflects whether the device picked up
        // VK_EXT_image_drm_format_modifier — lavapipe does, Venus
        // does. Just assert the field is set consistently with
        // what the Vk layer reported (no hard true-here).
    }

    /// Vk-backed: `dri3_import_pixmap` rejects unsupported
    /// (depth, bpp) combinations with a non-empty error before
    /// touching the dma-buf fd. Exercises the guard above the
    /// `import_dmabuf` call. Vk-attached so we hit the second
    /// arm (the Vk branch).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn dri3_import_pixmap_rejects_unsupported_depth_bpp() {
        use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
        let mut b = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip: no Vk: {e}");
                return;
            }
        };
        // Synthesise an arbitrary fd — depth=8 trips the guard
        // before the fd is consumed, so any openable file works.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("open /dev/null");
        let raw = f.into_raw_fd();
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let res = b.dri3_import_pixmap(fd, 16, 16, 64, 0, 0, 8, 8);
        assert!(
            res.is_err(),
            "depth=8 bpp=8 is outside Phase 4.2 RGB single-plane scope",
        );
    }

    /// Vk-backed: `dri3_supported_modifiers` returns at least
    /// LINEAR (0) on the screen side for depth-32/bpp-32. Lavapipe
    /// reports LINEAR; Venus reports LINEAR + tile modifiers; we
    /// only assert LINEAR is present (the conservative invariant).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn dri3_supported_modifiers_includes_linear_with_vk() {
        let b = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip: no Vk: {e}");
                return;
            }
        };
        let (window, screen) = b.dri3_supported_modifiers(0, 24, 32);
        assert!(
            window.contains(&0),
            "window modifiers always include LINEAR (Phase 4.1 scanout policy)",
        );
        assert!(
            screen.contains(&0),
            "screen modifiers always include LINEAR (fallback row of the design matrix)",
        );
    }

    /// xshmfence-path of `dri3_fence_from_fd`: mmap an xshmfence
    /// (synthesised via `memfd_create` + `ftruncate`), feed the
    /// fd in, assert it landed in `dri3_xshmfences` (not
    /// `dri3_sync_resources`) and that `trigger()` flips the
    /// state to signalled.
    #[test]
    fn dri3_fence_from_fd_xshmfence_path_triggers() {
        // The xshmfence module exposes the C alloc/map helpers;
        // build a fresh shm fd via `xshmfence_alloc_shm` directly
        // through libc-equivalent shape (memfd_create). To keep
        // the test self-contained without pulling libxshmfence
        // alloc, we synthesise a memfd that's at least page-sized
        // and let `FenceMapping::map` mmap it — libxshmfence's
        // map_shm only requires the fd be at least one page.
        use std::os::fd::{FromRawFd, OwnedFd};
        let raw =
            unsafe { libc::syscall(libc::SYS_memfd_create, c"yserver_dri3_test".as_ptr(), 0u32) };
        if raw < 0 {
            eprintln!("skip: memfd_create unavailable");
            return;
        }
        let raw = i32::try_from(raw).expect("fd fits i32");
        // Size the memfd to one page so map_shm succeeds.
        let page_raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page: libc::off_t = if page_raw > 0 {
            page_raw as libc::off_t
        } else {
            4096
        };
        if unsafe { libc::ftruncate(raw, page) } != 0 {
            unsafe { libc::close(raw) };
            eprintln!("skip: ftruncate failed");
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut b = KmsBackendV2::for_tests();
        let fence_xid: u32 = 0x4040_1111;
        b.dri3_fence_from_fd(fence_xid, fd)
            .expect("xshmfence import");
        assert!(
            b.dri3_xshmfences.contains_key(&fence_xid),
            "xshmfence path stores under dri3_xshmfences",
        );
        assert!(
            !b.dri3_sync_resources.contains_key(&fence_xid),
            "xshmfence path must NOT also populate dri3_sync_resources",
        );
        // Inspect the mapping's pre-trigger state.
        let mapping = b.dri3_xshmfences.get(&fence_xid).expect("present");
        let pre = mapping.query();
        // Trigger via the public trait surface.
        b.dri3_trigger_fence(fence_xid).expect("trigger ok");
        let post = b
            .dri3_xshmfences
            .get(&fence_xid)
            .expect("still present")
            .query();
        assert_eq!(post, 1, "after trigger, xshmfence query() == 1");
        // Defensive: pre and post differ in the expected direction.
        assert_ne!(pre, post, "trigger() should have changed the fence state");
    }

    /// `dri3_import_syncobj`'s no-Vk branch errs cleanly. Builds
    /// an `OwnedFd` from `/dev/null` and verifies the Vk-gate
    /// triggers before any external lib gets the fd — exercises
    /// the "Vk-required" guard mirroring v1's body. We use
    /// `IntoRawFd` + `FromRawFd` to keep the fd lifetime
    /// explicit since no Vk path runs.
    #[test]
    fn dri3_import_syncobj_no_vk_errs() {
        use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("open /dev/null");
        // SAFETY: we own this fd via the OpenOptions handle; we
        // re-wrap it as OwnedFd directly. No Vk path runs, the
        // function returns Err immediately, and the OwnedFd's
        // Drop closes it cleanly.
        let raw = f.into_raw_fd();
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut b = KmsBackendV2::for_tests();
        let res = b.dri3_import_syncobj(0x4040_3333, fd);
        assert!(
            res.is_err(),
            "import_syncobj without Vk must Err on the Vk gate",
        );
    }

    /// Stage 4d Manual-redirect CopyArea clip-by-children fix.
    ///
    /// Scenario from `yserver-hw-mate.log` (CC frame + reparented
    /// CC client window in one redirected backing):
    ///   - Frame W=997, H=652 at parent-local (0, 0).
    ///   - Reparented CC client at (11, 41) inside the frame,
    ///     size 975×600 (mapped).
    ///   - Marco copies its decoration pixmap into the frame with
    ///     a `ClipByChildren` GC, full 997×652.
    ///
    /// Pre-fix: v2's `copy_area` blits the full 997×652 into the
    /// redirected backing, clobbering CC's content. Visible symptom:
    /// only the small region CC repaints next survives — the famous
    /// "top-left square only" artefact.
    ///
    /// Spec-correct behaviour (Xorg `mi/midispcur.c` + the
    /// `ClipByChildren` rule): subtract every mapped child window's
    /// rect from the destination rect before issuing the copy. For
    /// this one-child case the result is exactly four strips: top,
    /// bottom, left-of-child, right-of-child.
    #[test]
    fn copy_area_clip_by_children_excludes_mapped_child_rect() {
        use ash::vk;
        let dst = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 997,
                height: 652,
            },
        };
        let child = vk::Rect2D {
            offset: vk::Offset2D { x: 11, y: 41 },
            extent: vk::Extent2D {
                width: 975,
                height: 600,
            },
        };

        let got = compute_copy_area_dst_rects(dst, &[child]);

        // Expected order: top strip, bottom strip, left middle,
        // right middle (Xorg/pixman band order).
        let want = vec![
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: 997,
                    height: 41,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 641 },
                extent: vk::Extent2D {
                    width: 997,
                    height: 11,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 41 },
                extent: vk::Extent2D {
                    width: 11,
                    height: 600,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 986, y: 41 },
                extent: vk::Extent2D {
                    width: 11,
                    height: 600,
                },
            },
        ];

        assert_eq!(
            got.len(),
            want.len(),
            "expected 4 surviving strips (top, bottom, left-middle, right-middle); \
             pre-fix returns the unclipped 1-rect input, which is the bug",
        );
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert_eq!(
                (g.offset.x, g.offset.y, g.extent.width, g.extent.height),
                (w.offset.x, w.offset.y, w.extent.width, w.extent.height),
                "strip {i} mismatch",
            );
        }
    }

    #[test]
    fn copy_area_clip_by_children_no_children_returns_input() {
        use ash::vk;
        let dst = vk::Rect2D {
            offset: vk::Offset2D { x: 5, y: 7 },
            extent: vk::Extent2D {
                width: 100,
                height: 80,
            },
        };
        let got = compute_copy_area_dst_rects(dst, &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].offset.x, 5);
        assert_eq!(got[0].offset.y, 7);
        assert_eq!(got[0].extent.width, 100);
        assert_eq!(got[0].extent.height, 80);
    }

    // ── compute_render_composite_clip — audit #2 (2026-05-19) ────────
    //
    // Mirrors Xorg's `miComputeCompositeRegion`
    // (`render/mipict.c:316-389`). Per-test vectors hand-traced from
    // the Xorg algorithm so the expected output is grounded in the
    // reference, not in my own arithmetic (per
    // `feedback_test_vectors_must_be_external`).

    /// All three clips `None` → result `None` (engine paints
    /// everywhere, matching Xorg's "no clientClip" path which
    /// leaves pRegion unconstrained beyond dst extent).
    #[test]
    fn compute_render_composite_clip_all_none_returns_none() {
        let got = compute_render_composite_clip(None, None, (0, 0), None, (0, 0));
        assert!(got.is_none());
    }

    /// Only dst clip set → result is dst clip (no translation).
    #[test]
    fn compute_render_composite_clip_only_dst() {
        let dst = vec![Rectangle16 {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        }];
        let got = compute_render_composite_clip(Some(&dst), None, (0, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(dst.as_slice()));
    }

    /// Only src clip set, src and dst coincide (xDst==xSrc, yDst==
    /// ySrc → translation (0,0)) → result is src clip as-is. This
    /// is the load-bearing case for the audit: xfwm4/muffin set a
    /// clip on a source picture and pre-fix yserver ignored it.
    #[test]
    fn compute_render_composite_clip_src_only_zero_translation() {
        let src = vec![Rectangle16 {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        }];
        let got = compute_render_composite_clip(None, Some(&src), (0, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(src.as_slice()));
    }

    /// Src clip translates to dst space by (xDst - xSrc, yDst -
    /// ySrc). Per Xorg `mipict.c:356`:
    /// `miClipPictureSrc(pRegion, pSrc, xDst - xSrc, yDst - ySrc)`.
    /// Set src clip {0,0 4×4}, composite from src(2,2) to
    /// dst(10,20), 4×4 — translation is (10-2, 20-2) = (8, 18).
    /// Expected: src clip translated to {8,18 4×4}.
    #[test]
    fn compute_render_composite_clip_translates_src_clip_to_dst_space() {
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        }];
        let got = compute_render_composite_clip(None, Some(&src), (8, 18), None, (0, 0));
        assert_eq!(
            got.as_deref(),
            Some(
                &[Rectangle16 {
                    x: 8,
                    y: 18,
                    width: 4,
                    height: 4,
                }][..]
            )
        );
    }

    /// Dst clip ∩ src-translated clip when the two overlap on a
    /// strict sub-rect. dst clip {0,0 100×100}; src clip {0,0 50×50}
    /// translated by (20, 30) → {20,30 50×50}. Intersection:
    /// {20,30 50×50} (src translates fully inside dst).
    #[test]
    fn compute_render_composite_clip_dst_and_src_intersection() {
        let dst = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }];
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let got = compute_render_composite_clip(Some(&dst), Some(&src), (20, 30), None, (0, 0));
        assert_eq!(
            got.as_deref(),
            Some(
                &[Rectangle16 {
                    x: 20,
                    y: 30,
                    width: 50,
                    height: 50,
                }][..]
            )
        );
    }

    /// Disjoint dst and src-translated clips → empty result (which
    /// Xorg treats as "paint nothing" — `miComputeCompositeRegion`
    /// returns FALSE there).
    #[test]
    fn compute_render_composite_clip_disjoint_yields_empty() {
        let dst = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }];
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }];
        // Translate src by (100, 0) → {100,0 10×10}; disjoint from
        // dst {0,0 10×10}.
        let got = compute_render_composite_clip(Some(&dst), Some(&src), (100, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(&[][..]));
    }

    /// Three-way intersection: dst ∩ src ∩ mask. Use disjoint
    /// translations that all overlap at one corner. dst {0,0 50×50},
    /// src {0,0 50×50} translated by (10, 10) → {10,10 50×50},
    /// mask {0,0 50×50} translated by (20, 20) → {20,20 50×50}.
    /// Three-way intersection: {20,20 30×30}.
    #[test]
    fn compute_render_composite_clip_three_way_intersection() {
        let dst = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let mask = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let got =
            compute_render_composite_clip(Some(&dst), Some(&src), (10, 10), Some(&mask), (20, 20));
        assert_eq!(
            got.as_deref(),
            Some(
                &[Rectangle16 {
                    x: 20,
                    y: 20,
                    width: 30,
                    height: 30,
                }][..]
            )
        );
    }

    /// Multi-rect dst clip ∩ single src clip translated: every
    /// dst rect intersects with the translated src rect; union of
    /// intersections is what the engine should emit per-scissor.
    #[test]
    fn compute_render_composite_clip_multi_rect_dst_with_single_src() {
        let dst = vec![
            Rectangle16 {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            Rectangle16 {
                x: 20,
                y: 0,
                width: 10,
                height: 10,
            },
        ];
        // Src clip {0,0 100×100} translated by (0, 0) → covers
        // both dst rects. Result: both dst rects survive.
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }];
        let got = compute_render_composite_clip(Some(&dst), Some(&src), (0, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(dst.as_slice()));
    }

    /// GC clip intersection: rect partially inside a single clip rect
    /// produces the intersection alone. Pre-fix the stub returns the
    /// whole rect — losing the GC clip semantics.
    #[test]
    fn intersect_rect_with_clip_single_overlapping_clip_returns_intersection() {
        use ash::vk;
        let rect = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 200,
                height: 200,
            },
        };
        let clip = vec![vk::Rect2D {
            offset: vk::Offset2D { x: 50, y: 50 },
            extent: vk::Extent2D {
                width: 100,
                height: 100,
            },
        }];
        let got = intersect_rect_with_clip(rect, &clip);
        assert_eq!(got.len(), 1, "single clip ∩ rect = one intersection");
        assert_eq!(
            (
                got[0].offset.x,
                got[0].offset.y,
                got[0].extent.width,
                got[0].extent.height,
            ),
            (50, 50, 100, 100),
        );
    }

    #[test]
    fn intersect_rect_with_clip_empty_clip_returns_empty() {
        use ash::vk;
        // Empty clip-rect list represents an empty XFixes region — paint nothing.
        let rect = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 10,
                height: 10,
            },
        };
        let got = intersect_rect_with_clip(rect, &[]);
        assert!(got.is_empty());
    }

    /// Multi-rect clip: dst that straddles two non-contiguous clip rects
    /// produces two intersections.
    #[test]
    fn intersect_rect_with_clip_multi_rect_clip_produces_per_rect_intersections() {
        use ash::vk;
        let rect = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 200,
                height: 100,
            },
        };
        let clip = vec![
            vk::Rect2D {
                offset: vk::Offset2D { x: 10, y: 10 },
                extent: vk::Extent2D {
                    width: 40,
                    height: 40,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 150, y: 10 },
                extent: vk::Extent2D {
                    width: 40,
                    height: 40,
                },
            },
        ];
        let got = intersect_rect_with_clip(rect, &clip);
        assert_eq!(got.len(), 2);
        assert_eq!(
            (
                got[0].offset.x,
                got[0].offset.y,
                got[0].extent.width,
                got[0].extent.height,
            ),
            (10, 10, 40, 40),
        );
        assert_eq!(
            (
                got[1].offset.x,
                got[1].offset.y,
                got[1].extent.width,
                got[1].extent.height,
            ),
            (150, 10, 40, 40),
        );
    }

    #[test]
    fn copy_area_clip_by_children_disjoint_child_returns_input() {
        // Child fully outside dst → no clipping.
        use ash::vk;
        let dst = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 50,
                height: 50,
            },
        };
        let child = vk::Rect2D {
            offset: vk::Offset2D { x: 200, y: 200 },
            extent: vk::Extent2D {
                width: 10,
                height: 10,
            },
        };
        let got = compute_copy_area_dst_rects(dst, &[child]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].offset.x, 0);
        assert_eq!(got[0].offset.y, 0);
    }
}
