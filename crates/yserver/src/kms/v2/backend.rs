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
    host_x11::{HostSubwindowConfig, HostSubwindowVisual, HostXidMap, PointerPosition},
    server::ServerState,
};
use yserver_protocol::x11::{ClipRectangles, FontMetrics, ResourceId, xfixes};

use crate::{
    drm,
    kms::{
        core::KmsCore,
        v2::{
            engine::{RenderEngine, decode_x11_pixel_bgra},
            platform::PlatformBackend,
            scene::SceneCompositor,
            store::{DrawableKind, DrawableStore},
            telemetry::Telemetry,
        },
    },
};

/// Per-window geometry tracked by v2's scene assembler. Stage 2 plan
/// Risk 3: a parallel `windows_v2` map on `KmsBackendV2` (NOT on
/// `KmsCore` — v1 doesn't need it). Stage 4 may collapse into
/// `KmsCore.windows` when `WindowState` splits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowGeometryV2 {
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    pub(crate) mapped: bool,
}

pub(crate) type WindowsV2Map = HashMap<u32, WindowGeometryV2>;

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
}

impl KmsBackendV2 {
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
        Ok(Self {
            core,
            platform,
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine,
            scene,
            windows_v2: WindowsV2Map::new(),
            telemetry: Telemetry::new(),
        })
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
        let mut base = Self::for_tests();
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
        Ok(base)
    }

    /// Headless test seed. Single 800×600 stub output; no
    /// Vulkan; no real DRM device. Mirrors `KmsBackend::for_tests`
    /// in shape so unit tests that drive v2 through
    /// `process_request` get a stable fixture.
    #[doc(hidden)]
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            core: KmsCore::for_tests(),
            platform: PlatformBackend::for_tests(),
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine: RenderEngine::stub(),
            scene: SceneCompositor::stub(),
            windows_v2: WindowsV2Map::new(),
            telemetry: Telemetry::new(),
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

    /// Allocate v2 storage + windows_v2 entry for a host xid.
    /// Idempotent against duplicate xids (logs + skips).
    fn allocate_window_storage(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        depth: u8,
    ) {
        if self.windows_v2.contains_key(&host_xid) {
            return;
        }
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
            },
        );
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
        let Some(target_id) = self.store.lookup(host_xid) else {
            return Ok(());
        };
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
                        dst_x: cursor_x + glyph.bitmap_left(),
                        dst_y: y - glyph.bitmap_top(),
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
            target_id,
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
        use crate::kms::v2::engine::decode_x11_pixel_bgra;

        if w <= 0 || h <= 0 {
            return Ok(());
        }
        let Some(target_id) = self.store.lookup(host_xid) else {
            return Ok(());
        };
        let color = decode_x11_pixel_bgra(background);
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x, y },
            extent: ash::vk::Extent2D {
                width: u32::try_from(w).unwrap_or(0),
                height: u32::try_from(h).unwrap_or(0),
            },
        };
        if let Err(e) =
            self.engine
                .fill_rect(&mut self.store, &mut self.platform, target_id, rect, color)
        {
            log::warn!("v2 image_text bg fill: engine.fill_rect xid={host_xid:#x}: {e:?}");
        } else {
            self.telemetry.record_paint_submit();
        }
        Ok(())
    }
}

/// Map a host-visual descriptor to a depth for the storage
/// allocator. Stage 2d picks BGRA32 for `CopyFromParent` (the
/// default visual is depth-24 ARGB-equivalent in our advertised
/// pixel format) and honours an explicit depth otherwise.
fn depth_for_visual(visual: HostSubwindowVisual) -> u8 {
    match visual {
        HostSubwindowVisual::CopyFromParent => 32,
        HostSubwindowVisual::Explicit { depth, .. } => {
            if depth == 0 {
                32
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
        None
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        None
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

    fn on_host_input(&mut self, _state: &mut ServerState, _ev: HostInputEvent) {
        // v2 input event dispatch isn't wired yet — there's no scene
        // for pointer-event fanout to target. Log once and drop the
        // event.
        self.log_v2_gap("on_host_input");
    }

    fn on_page_flip_ready(&mut self, _state: &mut ServerState) {
        let n = self.platform.outputs.len();
        for output_idx in 0..n {
            self.scene
                .handle_page_flip_complete(output_idx, &mut self.store, &mut self.platform);
            self.telemetry.record_frame_present();
        }
        // Sweep retired engine submits + retired drawables now
        // that their fences may have signaled.
        self.engine.poll_retired(&self.platform);
        self.store.poll_pending_retire(&mut self.platform);
    }

    fn mark_dirty(&mut self) {
        // Bump the scene-structure dirty bit; the next
        // `maybe_composite` will tick the compositor. Silent —
        // mark_dirty fires on every protocol mutation, so log
        // dedup would not help.
        self.scene.mark_scene_structure_dirty();
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
        self.log_v2_gap("dump_scanout");
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
        _host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _border_width: u16,
        visual: HostSubwindowVisual,
        _background_pixel: Option<u32>,
        _background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let xid = self.core.next_host_xid();
        let depth = depth_for_visual(visual);
        self.allocate_window_storage(xid, x, y, width.max(1), height.max(1), depth);
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
        if size_changed && let Some(old_id) = self.store.lookup(host_xid) {
            // Allocate fresh storage, decref the old. Stage 2d
            // doesn't preserve content across resize — clients
            // are expected to repaint after configure anyway
            // (X11 semantics).
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
                    }
                }
                Err(e) => {
                    log::warn!(
                        "v2 configure_subwindow: alloc storage failed for xid {host_xid:#x}: {e:?}",
                    );
                }
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        _host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.x = x;
            geom.y = y;
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _value_mask: u32,
        _values: &[u32],
    ) -> io::Result<()> {
        self.log_v2_gap("change_subwindow_attributes");
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _mask: u32,
        _enabled: bool,
    ) -> io::Result<()> {
        self.log_v2_gap("update_host_event_mask");
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
            self.allocate_window_storage(host_xid, 0, 0, 1, 1, 32);
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
            self.allocate_window_storage(host_xid, 0, 0, 1, 1, 32);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.core.xid_map.remove(&host_xid);
    }

    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        self.log_v2_gap("name_window_pixmap");
        Err(io::Error::other(
            "v2: name_window_pixmap not yet implemented",
        ))
    }

    fn allocate_redirected_backing(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window: WindowHandle,
        _width: u16,
        _height: u16,
        _depth: u8,
    ) -> io::Result<PixmapHandle> {
        // Stage 1b stub: no DrawableStore yet to allocate against.
        // Spec § "C-stubs special case": this is the deliberate
        // no-op cited in the plan — refcount lifecycle stays
        // consistent because the v1 path (alias_registry.insert)
        // never runs in v2.
        self.log_v2_gap("allocate_redirected_backing");
        Err(io::Error::other(
            "v2: allocate_redirected_backing not yet implemented",
        ))
    }

    fn release_redirected_backing(
        &mut self,
        _origin: Option<OriginContext>,
        _backing: PixmapHandle,
    ) -> io::Result<()> {
        // See `allocate_redirected_backing` — paired no-op. If
        // `alias_registry.decref` somehow returned `true` (it can't
        // in v2 since insert never runs), the caller would be told
        // "no storage to free" via this Ok return.
        self.log_v2_gap("release_redirected_backing");
        Ok(())
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
        self.log_v2_gap("create_cursor");
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
        self.log_v2_gap("create_glyph_cursor");
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
        self.log_v2_gap("define_cursor");
        Ok(())
    }

    // ── Container background ────────────────────────────────────

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        // Bookkeeping mutation — store the request in KmsCore so a
        // later v2 SceneCompositor can paint root with the right
        // colour. No paint side-effect in Stage 1b.
        self.core.bg_pixel = Some(pixel);
        self.core.bg_pixmap = None;
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        // Same shape as set_container_background_pixel. The handle
        // is opaque to v2 today — Stage 2 wires real backing storage.
        self.core.bg_pixmap = PixmapHandle::from_raw(host_pixmap_xid);
        self.core.bg_pixel = None;
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
        _host_pixmap: u32,
        _clip_x_origin: i16,
        _clip_y_origin: i16,
    ) -> io::Result<()> {
        // ClipState::Pixmap requires sampling the depth-1 pixmap, which
        // v2 can't do without DrawableStore. Log + leave clip cleared.
        self.log_v2_gap("set_clip_pixmap");
        self.core.current_clip = ClipState::None;
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.core.current_fill = FillState::Solid;
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _tile_x_origin: i16,
        _tile_y_origin: i16,
    ) -> io::Result<()> {
        self.log_v2_gap("set_gc_fill_tiled");
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
        let (Some(src), Some(dst)) = (
            self.store.lookup(src_host_xid),
            self.store.lookup(dst_host_xid),
        ) else {
            self.log_v2_gap("copy_area_unknown_xid");
            return Ok(());
        };
        let src_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(src_x),
                y: i32::from(src_y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        let dst_pos = ash::vk::Offset2D {
            x: i32::from(dst_x),
            y: i32::from(dst_y),
        };
        if let Err(e) = self.engine.copy_area(
            &mut self.store,
            &mut self.platform,
            src,
            dst,
            src_rect,
            dst_pos,
        ) {
            log::warn!(
                "v2 copy_area: engine.copy_area failed (src=0x{src_host_xid:x} \
                 dst=0x{dst_host_xid:x}): {e:?}",
            );
        } else {
            self.telemetry.record_paint_submit();
        }
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        _src_host_xid: u32,
        _dst_host_xid: u32,
        _src_x: i16,
        _src_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
        _plane: u32,
    ) -> io::Result<()> {
        self.log_v2_gap("copy_plane");
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
        let Some(id) = self.store.lookup(host_xid) else {
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
            id,
            ash::vk::Offset2D {
                x: i32::from(dst_x),
                y: i32::from(dst_y),
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
        let Some(id) = self.store.lookup(host_xid) else {
            self.log_v2_gap("get_image_unknown_xid");
            return Ok(None);
        };
        let depth = match self.store.get(id) {
            Some(d) => d.depth,
            None => return Ok(None),
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(x),
                y: i32::from(y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        let start = std::time::Instant::now();
        match self
            .engine
            .get_image(&mut self.store, &mut self.platform, id, rect, depth)
        {
            Ok(bytes) => {
                let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                self.telemetry.record_one_shot_submit();
                self.telemetry.record_fence_wait(ns);
                Ok(Some(bytes))
            }
            Err(e) => {
                log::warn!("v2 get_image: engine.get_image failed for xid {host_xid:#x}: {e:?}",);
                Ok(None)
            }
        }
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coordinate_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("poly_line");
        Ok(())
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _segments: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("poly_segment");
        Ok(())
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _rectangles: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("poly_rectangle");
        Ok(())
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _arcs: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("poly_arc");
        Ok(())
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coordinate_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("poly_point");
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
        let Some(id) = self.store.lookup(host_xid) else {
            self.log_v2_gap("poly_fill_rectangle_unknown_xid");
            return Ok(());
        };
        let color = decode_x11_pixel_bgra(foreground);
        for chunk in rectangles.chunks_exact(8) {
            let x = i16::from_le_bytes([chunk[0], chunk[1]]);
            let y = i16::from_le_bytes([chunk[2], chunk[3]]);
            let w = u16::from_le_bytes([chunk[4], chunk[5]]);
            let h = u16::from_le_bytes([chunk[6], chunk[7]]);
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(x),
                    y: i32::from(y),
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
            };
            if let Err(e) =
                self.engine
                    .fill_rect(&mut self.store, &mut self.platform, id, rect, color)
            {
                log::warn!(
                    "v2 poly_fill_rectangle: engine.fill_rect failed for xid {host_xid:#x}: {e:?}",
                );
                break;
            }
            self.telemetry.record_paint_submit();
        }
        Ok(())
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _arcs: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("poly_fill_arc");
        Ok(())
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coord_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("fill_poly");
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
        let Some(id) = self.store.lookup(host_xid) else {
            self.log_v2_gap("fill_rectangle_unknown_xid");
            return Ok(());
        };
        let color = decode_x11_pixel_bgra(foreground);
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(x),
                y: i32::from(y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        let res = self
            .engine
            .fill_rect(&mut self.store, &mut self.platform, id, rect, color);
        if let Err(e) = res {
            log::warn!("v2 fill_rectangle: engine.fill_rect failed for xid {host_xid:#x}: {e:?}",);
        } else {
            self.telemetry.record_paint_submit();
        }
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
        _host_drawable: AnyHandle,
        _ynest_format: u32,
        _value_mask: u32,
        _values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.log_v2_gap("render_create_picture");
        let xid = self.core.next_host_xid();
        Ok(PictureHandle::from_raw(xid))
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("render_change_picture");
        Ok(())
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
    ) -> io::Result<()> {
        self.log_v2_gap("render_free_picture");
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        self.log_v2_gap("render_create_glyphset");
        let xid = self.core.next_host_xid();
        Ok(GlyphSetHandle::from_raw(xid))
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
    ) -> io::Result<()> {
        self.log_v2_gap("render_free_glyphset");
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _body_tail: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("render_add_glyphs");
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _glyph_ids: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("render_free_glyphs");
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        _op: u8,
        _host_src: u32,
        _host_mask: u32,
        _host_dst: u32,
        _src_x: i16,
        _src_y: i16,
        _mask_x: i16,
        _mask_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
    ) -> io::Result<()> {
        self.log_v2_gap("render_composite");
        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _mask_fmt: u32,
        _host_gs: u32,
        _src_x: i16,
        _src_y: i16,
        _items: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        self.log_v2_gap("render_composite_glyphs");
        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_dst: u32,
        _op: u8,
        _color: [u8; 8],
        _rects: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        self.log_v2_gap("render_fill_rectangles");
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _traps: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        self.log_v2_gap("render_trapezoids");
        Ok(())
    }

    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _primitives: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        self.log_v2_gap("render_triangles_op");
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        _color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        self.log_v2_gap("render_create_solid_fill");
        let xid = self.core.next_host_xid();
        Ok(PictureHandle::from_raw(xid))
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.log_v2_gap("render_create_linear_gradient");
        let xid = self.core.next_host_xid();
        Ok(PictureHandle::from_raw(xid))
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.log_v2_gap("render_create_radial_gradient");
        let xid = self.core.next_host_xid();
        Ok(PictureHandle::from_raw(xid))
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_src_pic: PictureHandle,
        _x: u16,
        _y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        self.log_v2_gap("render_create_cursor");
        let xid = self.core.next_host_xid();
        Ok(CursorHandle::from_raw(xid))
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("render_set_picture_clip_rectangles");
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("render_set_picture_filter");
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        self.log_v2_gap("render_set_picture_transform");
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        // Advertise RENDER 0.11 (the version v1 reports). Stubbed
        // paint paths still need the version reply to flow through;
        // skipping it would break clients at extension query.
        Ok((0, 11))
    }

    // ── DRI3 — all unsupported in v2 Stage 1b (no Vulkan) ───────

    fn dri3_capabilities(&self) -> Dri3Caps {
        Dri3Caps::unsupported()
    }

    fn present_capabilities(&self, _window: u32) -> PresentCaps {
        PresentCaps::default()
    }

    // dri3_open / dri3_import_pixmap / dri3_export_pixmap /
    // dri3_fence_from_fd / dri3_fd_from_fence / dri3_import_syncobj
    // / dri3_free_syncobj / dri3_signal_syncobj /
    // dri3_supported_modifiers / dri3_trigger_fence keep the trait
    // defaults (which all return "DRI3 unsupported" errors).

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
            24 => Some(xkb_replies::reply_get_device_info()),
            4 | 12 | 13 | 15 | 19 | 21 | 22 | 23 | 101 => Some(xkb_replies::reply_minimal(minor)),
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
        self.log_v2_gap("xfixes_change_cursor_by_name");
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
        _max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<u8>> {
        self.log_v2_gap("list_fonts_proxy");
        // Minimal valid empty-list reply: 32-byte header, zero names.
        let mut reply = vec![0u8; 32];
        reply[0] = 1;
        Ok(reply)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        self.log_v2_gap("list_fonts_with_info_proxy");
        Ok(Vec::new())
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
        // Stub: 0 keysyms per code, count codes. Stage 2 wires real
        // xkbcommon-derived keysym tables.
        self.log_v2_gap("get_keyboard_mapping");
        let _ = (first_keycode, count);
        Ok((0, Vec::new()))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        // Stub: 0 keycodes per modifier.
        self.log_v2_gap("get_modifier_mapping");
        Ok((0, Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::KmsBackendV2;
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
        // No ARGB visual / colormap exposed.
        assert_eq!(b.argb_visual_xid(), None);
        assert_eq!(b.argb_colormap_xid(), None);
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
}
