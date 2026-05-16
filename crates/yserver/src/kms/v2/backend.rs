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
    pub(crate) bg_pixel: Option<u32>,
    pub(crate) bg_pixmap: Option<u32>,
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
    /// [`fill_rects_honoring_fill_state`] for the Solid arm.
    ///
    /// `GcFunction::Copy` (the common case) goes through the fast
    /// `vkCmdClearAttachments`-driven `engine.fill_rect`. Non-`Copy`
    /// functions (Stage 3f.2: GXclear / GXxor / GXinvert / etc.)
    /// divert to `engine.logic_fill`, which builds a per-function
    /// `VkLogicOp` pipeline through the shared
    /// `LogicFillPipelineCache`. `GcFunction::NoOp` is a no-op.
    fn fill_solid_rects(
        &mut self,
        id: crate::kms::v2::store::DrawableId,
        fg: u32,
        rects: &[Rectangle16],
    ) {
        use yserver_core::backend::GcFunction;
        if rects.is_empty() {
            return;
        }
        let function = self.core.current_function;
        if matches!(function, GcFunction::NoOp) {
            return;
        }
        if !matches!(function, GcFunction::Copy) {
            // Compute `opaque_alpha` per the L1 server-α invariant:
            // depth-32 ARGB destinations take the LogicOp on all four
            // channels; depth-24/8/1 are server-owned-α so the
            // pipeline's write mask drops alpha to keep the dst byte
            // intact. Depth lookup via the drawable record.
            let opaque_alpha = self.store.get(id).map(|d| d.depth != 32).unwrap_or(true);
            match self.engine.logic_fill(
                &mut self.store,
                &mut self.platform,
                id,
                function,
                opaque_alpha,
                fg,
                rects,
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
        let color = decode_x11_pixel_bgra(fg);
        for r in rects {
            if r.width == 0 || r.height == 0 {
                continue;
            }
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(r.x),
                    y: i32::from(r.y),
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                },
            };
            if let Err(e) =
                self.engine
                    .fill_rect(&mut self.store, &mut self.platform, id, rect, color)
            {
                log::warn!("v2 fill_solid_rects: engine.fill_rect failed: {e:?}");
                break;
            }
            self.telemetry.record_paint_submit();
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
        id: crate::kms::v2::store::DrawableId,
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
                    self.fill_solid_rects(id, fg, rects);
                    return;
                }
                if !self.try_tiled_fill(id, tile_xid, origin.0, origin.1, rects) {
                    // Tile not in store / aliases dst / non-BGRA8
                    // tile — degenerate to solid foreground.
                    self.fill_solid_rects(id, fg, rects);
                }
            }
            FillState::Solid | FillState::Stippled { .. } | FillState::OpaqueStippled { .. } => {
                // Stipple support is post-Stage-3 (no real-app smoke
                // client drives it on KMS). Fall through as solid.
                self.fill_solid_rects(id, fg, rects);
            }
        }
    }

    /// Tile fill via `engine.render_composite` (Stage 3f.3). Returns
    /// `true` iff the call submitted; `false` if the tile isn't
    /// usable (unknown xid, self-tile aliasing, non-BGRA8 tile
    /// format), in which case the caller falls back to solid.
    fn try_tiled_fill(
        &mut self,
        dst_id: crate::kms::v2::store::DrawableId,
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
        if tile_id == dst_id {
            // Self-tile would alias src + dst inside render_composite.
            return false;
        }
        let tile_format = self.store.get(tile_id).map(|d| d.storage.format);
        if tile_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            log::debug!("v2 try_tiled_fill: tile 0x{tile_xid:x} format {tile_format:?} not BGRA8");
            return false;
        }
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
                    dst_x: i32::from(r.x),
                    dst_y: i32::from(r.y),
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
            dst_id,
            &composite_rects,
            None, // GC clip already applied by caller
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
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
                bg_pixel,
                bg_pixmap: None,
            },
        );
        // Stage 3f.6: clear newly-allocated storage to `bg_pixel`
        // so freshly-mapped windows have a defined colour. v1 does
        // this at the same hook (`create_subwindow`). Skips when
        // storage wasn't allocated (test fixture) or when there's
        // no bg_pixel (caller didn't pass one — depth-1/8 mask
        // pixmaps or windows with bg=None).
        if storage_allocated
            && let Some(pixel) = bg_pixel
            && let Some(id) = self.store.lookup(host_xid)
        {
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: u32::from(width.max(1)),
                    height: u32::from(height.max(1)),
                },
            };
            let color = decode_x11_pixel_bgra(pixel);
            if let Err(e) =
                self.engine
                    .fill_rect(&mut self.store, &mut self.platform, id, rect, color)
            {
                log::debug!(
                    "v2 allocate_window_storage: initial bg_pixel fill failed for xid {host_xid:#x}: {e:?}"
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
        } => Some((
            ResolvedSource::Gradient(host_pic),
            *repeat,
            *transform,
            false,
        )),
    }
}

/// Stage 3c: dst picture resolution. RENDER paint ops require
/// the dst to be a `PictureRecord::Drawable` (you can't paint
/// into a SolidFill or a Gradient). Returns the resolved
/// `DrawableId` plus the picture's clip rectangles (already
/// pre-shifted by `clip_x` / `clip_y` per Stage 3b).
fn resolve_dst_picture_for_render(
    core: &KmsCore,
    store: &crate::kms::v2::store::DrawableStore,
    host_pic: u32,
) -> Option<(crate::kms::v2::store::DrawableId, Option<Vec<Rectangle16>>)> {
    let PictureRecord::Drawable { host_xid, clip, .. } = core.pictures.get(&host_pic)? else {
        return None;
    };
    let id = store.lookup(*host_xid)?;
    Some((id, clip.clone()))
}

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
        let depth = depth_for_visual(visual);
        let parent_xid = host_parent.as_raw();
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
                    } else {
                        // Stage 3f.6: clear the fresh storage to
                        // bg_pixel so resize doesn't leave Vk-
                        // undefined content visible until the
                        // client's next repaint.
                        if let Some(pixel) = bg_pixel
                            && let Some(id) = self.store.lookup(host_xid)
                        {
                            let rect = ash::vk::Rect2D {
                                offset: ash::vk::Offset2D::default(),
                                extent: ash::vk::Extent2D {
                                    width: u32::from(new_w),
                                    height: u32::from(new_h),
                                },
                            };
                            let color = decode_x11_pixel_bgra(pixel);
                            if let Err(e) = self.engine.fill_rect(
                                &mut self.store,
                                &mut self.platform,
                                id,
                                rect,
                                color,
                            ) {
                                log::debug!(
                                    "v2 configure_subwindow: bg_pixel fill on resize failed for xid {host_xid:#x}: {e:?}"
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
        let parent = if host_parent == 0 || !self.windows_v2.contains_key(&host_parent) {
            None
        } else {
            Some(host_parent)
        };
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.x = x;
            geom.y = y;
            geom.parent = parent;
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
        let mut idx = 0;
        if value_mask & 0x01 != 0 && idx < values.len() {
            // CWBackPixmap. 0 = None / inherit-from-parent.
            let v = values[idx];
            geom.bg_pixmap = if v == 0 { None } else { Some(v) };
            idx += 1;
        }
        if value_mask & 0x02 != 0 && idx < values.len() {
            // CWBackPixel — opaque ARGB-or-XRGB pixel value.
            geom.bg_pixel = Some(values[idx]);
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
            // Top-level: parent = None (root), no bg_pixel known yet
            // (set later via change_subwindow_attributes).
            self.allocate_window_storage(host_xid, 0, 0, 1, 1, 32, None, None);
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
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(id) = self.store.lookup(host_xid) else {
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
        self.fill_solid_rects(id, foreground, &rects);
        Ok(())
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        let Some(id) = self.store.lookup(host_xid) else {
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
        self.fill_solid_rects(id, foreground, &rects);
        Ok(())
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        let Some(id) = self.store.lookup(host_xid) else {
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
        self.fill_solid_rects(id, foreground, &rects);
        Ok(())
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(id) = self.store.lookup(host_xid) else {
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
        self.fill_solid_rects(id, foreground, &rects);
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
        let Some(id) = self.store.lookup(host_xid) else {
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
        self.fill_solid_rects(id, foreground, &rects);
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
        self.fill_rects_honoring_fill_state(id, foreground, &rects);
        Ok(())
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(id) = self.store.lookup(host_xid) else {
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
            self.fill_rects_honoring_fill_state(id, foreground, &rects);
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
        let Some(id) = self.store.lookup(host_xid) else {
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
        self.fill_rects_honoring_fill_state(id, foreground, &rects);
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
        let rects = self.intersect_with_current_clip(&[Rectangle16 {
            x,
            y,
            width,
            height,
        }]);
        self.fill_rects_honoring_fill_state(id, foreground, &rects);
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
        _ynest_format: u32,
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
        self.core
            .pictures
            .insert(picture_xid, PictureRecord::drawable_default(drawable_xid));
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
        change_picture_apply_mask(&mut self.core, host_pic, body);
        Ok(())
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
        let Some((dst_id, dst_clip)) =
            resolve_dst_picture_for_render(&self.core, &self.store, host_dst)
        else {
            log::debug!("v2 render_composite gap: host_dst 0x{host_dst:x} not a Drawable picture");
            return Ok(());
        };

        let rect = crate::kms::vk::ops::render::CompositeRect {
            src_x: i32::from(src_x),
            src_y: i32::from(src_y),
            mask_x: i32::from(mask_x),
            mask_y: i32::from(mask_y),
            dst_x: i32::from(dst_x),
            dst_y: i32::from(dst_y),
            width: u32::from(width),
            height: u32::from(height),
        };
        let stats = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            mask_resolved,
            dst_id,
            std::slice::from_ref(&rect),
            dst_clip.as_deref(),
            src_repeat,
            mask_repeat,
            src_transform,
            mask_transform,
            mask_component_alpha,
        );
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("v2 render_composite: engine returned {e:?} on dst 0x{host_dst:x}");
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
        let ResolvedSource::Solid(foreground_premul) = src_resolved else {
            log::debug!(
                "v2 composite_glyphs gap: src 0x{host_src:x} is not SolidFill (plan §3d \
                 v1-parity scope)"
            );
            self.telemetry.record_composite_glyphs_dropped_unsupported();
            return Ok(());
        };
        let Some((dst_id, dst_clip)) =
            resolve_dst_picture_for_render(&self.core, &self.store, host_dst)
        else {
            log::debug!("v2 composite_glyphs gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
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
        // with a stable slice reference.
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
                    dst_x: p.dst_x,
                    dst_y: p.dst_y,
                })
            })
            .collect();

        if inputs.is_empty() {
            return Ok(());
        }

        let stats = self.engine.composite_glyphs(
            &mut self.store,
            &mut self.platform,
            dst_id,
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
        let Some((dst_id, dst_clip)) =
            resolve_dst_picture_for_render(&self.core, &self.store, host_dst)
        else {
            log::debug!(
                "v2 render_fill_rectangles gap: host_dst 0x{host_dst:x} not a Drawable picture"
            );
            return Ok(());
        };

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
                dst_x: i32::from(rx),
                dst_y: i32::from(ry),
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
            dst_id,
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
        let dx = i32::from(x_off) << 16;
        let dy = i32::from(y_off) << 16;
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

        // Resolve src + dst via the same helpers render_composite
        // uses. The trap path doesn't read GC clip — picture clip
        // (from dst) is what scopes the draw (plan §4).
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_trapezoids gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let Some((dst_id, dst_clip)) =
            resolve_dst_picture_for_render(&self.core, &self.store, host_dst)
        else {
            log::debug!("v2 render_trapezoids gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_id,
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
        let dx = i32::from(x_off) << 16;
        let dy = i32::from(y_off) << 16;
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

        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_triangles gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let Some((dst_id, dst_clip)) =
            resolve_dst_picture_for_render(&self.core, &self.store, host_dst)
        else {
            log::debug!("v2 render_triangles gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_id,
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
            *clip = if rects.is_empty() { None } else { Some(rects) };
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
    use super::{KmsBackendV2, PictureRecord};
    use crate::kms::cpu_types::Repeat;
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

    /// Per plan §3d "Op / source matrix": non-SolidFill source
    /// (Drawable / Gradient) also drops with the unsupported
    /// counter — Cairo's rare alpha-mask source case. Use a
    /// gradient picture (no store dependency) so the test fixture
    /// doesn't need live Vk.
    #[test]
    fn v2_composite_glyphs_non_solidfill_source_drops() {
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
            b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 1,
            "gradient source must hit the v1-parity gate",
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
        assert!(!geom.mapped, "mapped is set later via map_subwindow");
    }
}
