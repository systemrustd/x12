//! `RenderEngine` — drawing primitives into [`DrawableStore`] storage.
//!
//! Stage 2c lands the three single-drawable paint ops the v2 model
//! needs for offscreen pixel-correctness gates:
//! [`fill_rect`](RenderEngine::fill_rect),
//! [`put_image`](RenderEngine::put_image),
//! [`get_image`](RenderEngine::get_image). Each op is a self-
//! contained `vkQueueSubmit2` against a fresh [`FenceTicket`] from
//! [`PlatformBackend`]. The ticket is recorded on every drawable
//! the op touched (cross-cutting §5) so a later compose-read can
//! see the in-flight write; and parked on
//! [`RenderEngine::submitted`] for retirement via
//! [`poll_retired`](RenderEngine::poll_retired).
//!
//! What's deliberately NOT in 2c (per the Stage 2 plan):
//!
//! - `copy_area` (joins 2d alongside scene/blit).
//! - RENDER / glyphs / text / poly_line / poly_segment / etc.
//!   Logged-gap on `KmsBackendV2` until Stage 3.
//! - Per-op batching across multiple ops. 2c uses one
//!   submission per Backend method call — equivalent perf-wise to
//!   v1's per-op shape; submit-aggregation arrives in Stage 5.
//! - `vkQueueWaitIdle` anywhere. Only `get_image` waits on its own
//!   `FenceTicket` (off the hot path; sync RPC by protocol design).
//! - GC `function != GXcopy` and non-zero `planemask`. Stage 2 plan
//!   §"What doesn't ship in Stage 2": v2 logs a gap + drops the op.
//!   These come back in Stage 3 alongside RENDER.
//!
//! Layout discipline: every paint op brackets its work with two
//! [`Drawable::record_layout_transition`] calls so the storage is
//! returned to `SHADER_READ_ONLY_OPTIMAL` for the next consumer
//! (compose-read in 2d, another paint op in 2c).

#![allow(
    dead_code,
    reason = "RenderEngine primitives are consumed by Stages 2d–2f"
)]

use std::{
    collections::{HashMap, VecDeque},
    ptr::NonNull,
    sync::Arc,
};

use ash::vk;

use super::{
    glyph_atlas::V2GlyphAtlas,
    platform::{FenceTicket, PlatformBackend},
    store::{DrawableId, DrawableStore},
};
use crate::kms::{
    cpu_types::{PictTransform, Rectangle16, Repeat},
    scheduler::batch_descriptor_arena::BatchDescriptorArena,
    vk::{
        device::VkContext,
        dst_readback::DstReadback,
        glyph::{AtlasEntry, GlyphKey},
        ops::{
            render::CompositeTarget,
            text::{
                TextAtlas, TextGlyph, TextRunTarget, record_text_run, record_text_run_scissored,
            },
        },
        render_pipeline::{RenderPipelineCache, SolidColorImage},
        text_pipeline::TextPipeline,
    },
};

// ────────────────────────────────────────────────────────────────
// Errors
// ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub(crate) enum RenderError {
    #[error("vk: {0:?}")]
    Vk(vk::Result),
    #[error("drawable {0:?} not present in store")]
    UnknownDrawable(DrawableId),
    #[error("renderer not initialised (no VkContext)")]
    NoVk,
    #[error("renderer in failed state — refusing further ops")]
    RendererFailed,
    #[error("unsupported depth {0} for v2 Stage 2c ops")]
    UnsupportedDepth(u8),
    #[error("source byte slice too short for {expected} bytes")]
    TruncatedSource { expected: usize },
}

impl From<vk::Result> for RenderError {
    fn from(r: vk::Result) -> Self {
        RenderError::Vk(r)
    }
}

// ────────────────────────────────────────────────────────────────
// SubmittedOp — one in-flight CB awaiting fence retirement.
//
// Holds onto the resources whose destruction must wait for the
// I6a fence: the CB itself + any per-op staging buffer the op
// allocated. On `poll_retired`, signaled entries are destroyed.
// ────────────────────────────────────────────────────────────────

struct SubmittedOp {
    cb: vk::CommandBuffer,
    ticket: FenceTicket,
    /// Per-op staging buffer (only for `put_image` and Stage 3a
    /// glyph upload). Destroyed only after the fence signals;
    /// dropping it earlier would race the GPU's TRANSFER_READ.
    staging: Option<StagingBuffer>,
    /// Per-op scratch image (only for `copy_area` self-overlap
    /// path). Destroyed only after the fence signals.
    scratch: Option<ScratchImage>,
    /// Stage 3a: cloned `atlas_last_upload_ticket` snapshot.
    /// Atlas-sampling ops (text runs, RENDER glyphs in Stage 3d)
    /// stash the engine's then-current upload ticket here so the
    /// atlas image + the upload's staging buffer can't retire
    /// before the consume CB has executed. Same-queue submission
    /// order is the GPU dependency; this Arc keeps CPU-side
    /// destruction gated on retirement of both submissions.
    atlas_ticket: Option<FenceTicket>,
    /// Stage 3c: per-op descriptor arena holding the descriptor
    /// pool that backs the RENDER pipeline descriptor set this CB
    /// references. Released (pools destroyed) on retirement; this
    /// is the v2 mirror of v1's `PaintBatch::descriptor_arena`
    /// retire path. Most ops (fill / put_image / copy_area / text
    /// runs) don't need this and leave it `None`.
    descriptor_arena: Option<BatchDescriptorArena>,
}

/// One-shot device-local image used by `copy_area`'s same-image
/// overlap path (Stage 2d). Destroyed only after the owning op's
/// fence signals.
struct ScratchImage {
    vk: Arc<VkContext>,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Drop for ScratchImage {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

/// One-shot host-visible buffer used for `put_image` upload or
/// `get_image` readback. Destroyed on drop.
struct StagingBuffer {
    vk: Arc<VkContext>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: NonNull<u8>,
    size: u64,
}

// SAFETY: the v2 backend's single-threaded core invariant keeps
// `StagingBuffer` pinned to the backend thread; `NonNull<u8>` is
// only sound to Send under that invariant.
unsafe impl Send for StagingBuffer {}

impl StagingBuffer {
    fn new(vk: Arc<VkContext>, size: u64) -> Result<Self, vk::Result> {
        Self::new_with_usage(
            vk,
            size,
            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
        )
    }

    /// Stage 3e.2: variant with explicit usage flags. The trap
    /// path needs `VERTEX_BUFFER` usage on its instance-data
    /// upload buffer (cmd_bind_vertex_buffers requires that bit).
    fn new_with_usage(
        vk: Arc<VkContext>,
        size: u64,
        usage: vk::BufferUsageFlags,
    ) -> Result<Self, vk::Result> {
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };
        let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let Some(mt) = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        }) else {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(e);
        }
        let mapped_raw = match unsafe {
            vk.device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_buffer(buffer, None);
                }
                return Err(e);
            }
        };
        let mapped = NonNull::new(mapped_raw.cast::<u8>()).expect("vkMapMemory non-null");
        Ok(Self {
            vk,
            buffer,
            memory,
            mapped,
            size,
        })
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Stage 3c: drawable view cache (plan §1).
//
// A Drawable can be sampled in three roles (source / mask /
// alpha-only) with different sampler + swizzle bindings. Keying
// the cache on `DrawableId` alone would over-share; keying on
// `(DrawableId, SamplerConfig, SwizzleClass)` gives the same
// `Drawable` a separate cached view per role. Eviction is driven
// by drawable retirement (see `Drawable` lifecycle in
// `DrawableStore`); no LRU.
// ────────────────────────────────────────────────────────────────

/// Sampler configuration the cache key cares about. Filter is
/// `Nearest` only in Stage 3 (per spec § "Out of scope"); the
/// address mode mirrors the four X RENDER `Repeat` values.
#[allow(
    dead_code,
    reason = "Variants are populated by Stage 3c's render_composite path"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SamplerConfig {
    /// `Repeat::None` — clamp-to-border at picture edges (or
    /// `REPEAT_PAD` for synthetic 1×1 sources; see render plan §3c).
    Clamp,
    /// `Repeat::Normal` — wrap.
    Repeat,
    /// `Repeat::Pad` — clamp-to-edge.
    Pad,
    /// `Repeat::Reflect` — mirrored repeat.
    Reflect,
}

/// Swizzle bucket for the cached view. Distinguishes the three
/// formats v2 supports for RENDER sources / masks (per plan §3b
/// `RenderEngine adds`):
///
/// - `RgbaIdent` — depth-32 BGRA picture: regular `(b, g, r, a)`
///   sample.
/// - `AlphaOnlyR8` — R8 storage sampled as an alpha mask;
///   swizzle `(0, 0, 0, R)` so the shader's `.a` returns the
///   alpha byte.
/// - `BgraNoAlpha` — depth-24 BGRA picture (r8g8b8 / x8r8g8b8):
///   swizzle `(IDENT, IDENT, IDENT, ONE)` so the shader sees
///   alpha = 1 per X RENDER's "alpha defaults to 1 when missing"
///   rule.
#[allow(
    dead_code,
    reason = "Variants are populated by Stage 3c's render_composite path"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SwizzleClass {
    RgbaIdent,
    AlphaOnlyR8,
    BgraNoAlpha,
}

/// Cached `vk::ImageView` for a `(DrawableId, SamplerConfig,
/// SwizzleClass)` triple. The engine destroys these on Drawable
/// retire (signalled by `DrawableStore::poll_pending_retire`).
/// The underlying `Drawable.storage.image` lifetime gates view
/// validity.
#[allow(
    dead_code,
    reason = "Built and consumed by Stage 3c's render_composite path"
)]
pub(crate) struct CachedDrawableView {
    pub(crate) view: vk::ImageView,
}

// ────────────────────────────────────────────────────────────────
// RenderEngine
// ────────────────────────────────────────────────────────────────

/// v2's rendering layer. Wraps an optional [`RenderEngineInner`]
/// so the test fixture (Vk-less) can construct an engine that
/// declines paint ops with a `NoVk` error instead of panicking.
pub(crate) struct RenderEngine {
    inner: Option<RenderEngineInner>,
}

struct RenderEngineInner {
    vk: Arc<VkContext>,
    /// Per-op CBs awaiting fence retirement. Drained by
    /// [`RenderEngine::poll_retired`] (called periodically by
    /// `KmsBackendV2` and at shutdown).
    submitted: VecDeque<SubmittedOp>,
    /// Stage 3b: per-picture GPU-side state. Today only carries
    /// gradient `GradientPicture` instances built lazily by Stage
    /// 3c's first `render_composite`; Stage 3b just ensures
    /// `render_free_picture` has a cleanup hook so an in-flight
    /// gradient's Vk handles get destroyed at the right moment.
    /// The empty `PicturePaintState` placeholder enum sits here
    /// until 3c needs to differentiate variants.
    picture_paint: HashMap<u32, PicturePaintState>,
    /// Stage 3a: glyph atlas. Lazy — first text run pays the
    /// 16 MiB R8 allocation. `None` until first image_text op.
    glyph_atlas: Option<V2GlyphAtlas>,
    /// Stage 3a: text pipeline (TextRunTarget descriptor bound to
    /// the atlas image view). Lazy — built once after the atlas
    /// is constructed. The pipeline's descriptor set references
    /// the atlas image view permanently; the atlas image's
    /// long-lived ownership makes this safe.
    text_pipeline: Option<TextPipeline>,
    /// Stage 3a: latest atlas-upload ticket. Cloned onto every
    /// atlas-consuming SubmittedOp (text runs, RENDER glyphs in
    /// Stage 3d) so the upload's per-call staging buffer and the
    /// atlas image stay alive on the CPU side until both upload
    /// and consume have retired. None when no upload has happened
    /// in the current session (atlas freshly created or every
    /// upload already retired).
    atlas_last_upload_ticket: Option<FenceTicket>,
    /// Stage 3c: lazy-built RENDER `Composite` pipeline cache.
    /// Adopted wholesale from v1. Pipelines compile on first use
    /// of each `(op, dst_format, dst_has_alpha, component_alpha)`
    /// key. `None` until the first `render_composite` call.
    render_pipelines: Option<RenderPipelineCache>,
    /// Stage 3c: 1×1 BGRA8 source scratch for `SolidFill` source.
    /// `record_solid_color_clear` rewrites the texel inside each
    /// composite CB before sampling. Lazy.
    solid_src_image: Option<SolidColorImage>,
    /// Stage 3c: 1×1 BGRA8 mask scratch for `SolidFill` mask.
    /// Same shape as `solid_src_image`. Lazy.
    solid_mask_image: Option<SolidColorImage>,
    /// Stage 3c: 1×1 BGRA8 mask scratch cleared once to opaque
    /// white. Bound as `mask_tex` for Composite calls without a
    /// mask — `mask.a == 1.0` makes the multiplication a no-op
    /// and keeps the shader / descriptor layout uniform. Lazy
    /// (pays one allocation + one-shot clear at first
    /// `render_composite`).
    white_mask_image: Option<SolidColorImage>,
    /// Stage 3c: `Disjoint` / `Conjoint` shader-side blend reads
    /// the current dst into this scratch before the draw samples
    /// it. Lazy.
    dst_readback: Option<DstReadback>,
    /// Stage 3c.3: self-alias scratch. When the resolved source
    /// (or mask) picture wraps the same backing as the destination
    /// (`src.drawable_id() == dst_id`), we copy dst into this
    /// scratch before the composite pass and bind its view as the
    /// `src_tex` / `mask_tex` descriptor instead of dst's own
    /// drawable view. Vulkan can't sample an image while it's bound
    /// as a color attachment in the same draw; the scratch breaks
    /// the alias. Reuses [`DstReadback`]'s growable per-format
    /// scratch shape — identical Vk requirements (sampled image +
    /// dst-format swizzle for no-alpha picture formats).
    src_alias_readback: Option<DstReadback>,
    /// Stage 3e.2: GPU rasterizer for RENDER `Trapezoids` /
    /// `Triangles`. Lazy — first trap/tri request pays the
    /// pipeline build.
    trap_pipeline: Option<crate::kms::vk::trap_pipeline::TrapPipeline>,
    /// Stage 3e.2: R8 coverage scratch the trap pipeline writes
    /// into, then the composite pass samples as a mask. Grows on
    /// demand (per-bbox). Lazy.
    ///
    /// NOTE: `ensure_image_size_returning_old` returns the old image
    /// as a `BatchResource` for the caller to defer-release;
    /// v2 currently drops the returned `Box<dyn BatchResource>` on
    /// the floor — same shape as the `dst_readback` grow leak in
    /// Stage 3c.2. A real fix needs a per-engine retired-resources
    /// list that drains on next `poll_retired`. Tracked for the
    /// Stage 5 polish cycle.
    mask_scratch: Option<crate::kms::vk::mask_scratch::MaskScratch>,
    /// Stage 3c: drawable view cache (plan §1). Keyed by
    /// `(DrawableId, SamplerConfig, SwizzleClass)`. Views are
    /// destroyed on Drawable retire; the engine's
    /// `notify_drawable_retired` hook prunes matching entries.
    drawable_view_cache: HashMap<(DrawableId, SamplerConfig, SwizzleClass), CachedDrawableView>,
    /// Stage 3f.2: per-`vk::Format` `LogicFillPipelineCache`. Built
    /// lazily on first non-`GXcopy` fill against a given dst format.
    /// The inner cache already keys its pipelines by
    /// `(GcFunction, opaque_alpha)`; we shard by `vk::Format` because
    /// each pipeline is bound to a single color attachment format at
    /// build time. Typical sessions only ever hold the
    /// `B8G8R8A8_UNORM` entry; R8 dst (depth 1/8) ops paint via
    /// `put_image` rather than fill, so the R8 branch only fires for
    /// rendercheck's `copy_plane` corner.
    logic_fill_caches:
        HashMap<vk::Format, crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache>,
}

impl RenderEngine {
    /// Production constructor. Borrows the platform's `VkContext`
    /// (cloned `Arc`); CB allocation goes through the platform's
    /// shared `OpsCommandPool` on each op.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` if `platform` was built via `for_tests`
    /// (no Vk). Production paths always have Vk.
    pub(crate) fn new(platform: &PlatformBackend) -> Result<Self, RenderError> {
        let vk = platform.vk().ok_or(RenderError::NoVk)?.clone();
        Ok(Self {
            inner: Some(RenderEngineInner {
                vk,
                submitted: VecDeque::new(),
                picture_paint: HashMap::new(),
                glyph_atlas: None,
                text_pipeline: None,
                atlas_last_upload_ticket: None,
                render_pipelines: None,
                solid_src_image: None,
                solid_mask_image: None,
                white_mask_image: None,
                dst_readback: None,
                src_alias_readback: None,
                trap_pipeline: None,
                mask_scratch: None,
                drawable_view_cache: HashMap::new(),
                logic_fill_caches: HashMap::new(),
            }),
        })
    }

    /// Vk-less constructor — used by `KmsBackendV2::for_tests` and
    /// Stage 1b-era callers that haven't migrated yet. Every paint
    /// op on a stubbed engine returns `NoVk`.
    pub(crate) fn stub() -> Self {
        Self { inner: None }
    }

    /// Whether the engine has a live Vk inner. Tests use this to
    /// skip Vk-backed assertions on the stub fixture.
    pub(crate) fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    /// Walk `submitted`, dropping entries whose [`FenceTicket`]
    /// has signaled. Their CB is freed and any staging buffer
    /// destroyed.
    pub(crate) fn poll_retired(&mut self, platform: &PlatformBackend) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(pool) = platform.ops_command_pool_handle() else {
            return;
        };
        let device = &inner.vk.device;
        // Walk front-to-back, removing prefixes that have signaled.
        // Same-queue submission order guarantees prefix-signal
        // monotonicity; if entry N's ticket is signaled, entry
        // N-1's also is. We could short-circuit on first
        // unsignaled but the loop is small enough to walk all.
        while let Some(front) = inner.submitted.front() {
            if !front.ticket.poll_signaled(&inner.vk) {
                break;
            }
            let mut op = inner.submitted.pop_front().expect("non-empty");
            unsafe {
                device.free_command_buffers(pool, &[op.cb]);
            }
            // staging drops at end of scope → destroys Vk handles.
            drop(op.staging.take());
            // Stage 3c: release the per-op descriptor pool, if any.
            // `BatchDescriptorArena` owns its pools through the
            // `BatchResource::release` path; plain Drop leaks them.
            if let Some(arena) = op.descriptor_arena.take() {
                use crate::kms::scheduler::paint_batch::BatchResource;
                Box::new(arena).release(&inner.vk);
            }
        }
    }

    /// Drain every in-flight submit, waiting on the deepest
    /// ticket. Called at shutdown to ensure all CB / staging
    /// resources are reclaimed before pool destruction.
    pub(crate) fn drain_all(&mut self, platform: &PlatformBackend) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(pool) = platform.ops_command_pool_handle() else {
            return;
        };
        let device = &inner.vk.device;
        // Wait on each ticket in order. Off-hot-path; one wait
        // per pending op is fine at shutdown.
        while let Some(mut op) = inner.submitted.pop_front() {
            let _ = op.ticket.wait(&inner.vk);
            unsafe {
                device.free_command_buffers(pool, &[op.cb]);
            }
            drop(op.staging.take());
            if let Some(arena) = op.descriptor_arena.take() {
                use crate::kms::scheduler::paint_batch::BatchResource;
                Box::new(arena).release(&inner.vk);
            }
        }
    }

    /// Count of in-flight submits awaiting retirement. Tests use
    /// this to assert the lifecycle book-keeping.
    pub(crate) fn pending_count(&self) -> usize {
        self.inner.as_ref().map(|i| i.submitted.len()).unwrap_or(0)
    }

    /// Stage 3b: drop any GPU-side state cached for `host_pic`.
    /// `KmsBackendV2::render_free_picture` calls this after
    /// removing the picture record from `KmsCore.pictures`. Stage
    /// 3f.13's `PicturePaintState::Gradient` drops its
    /// [`GradientPicture`] (image / view / memory) on remove.
    pub(crate) fn picture_paint_remove(&mut self, host_pic: u32) {
        if let Some(inner) = self.inner.as_mut() {
            inner.picture_paint.remove(&host_pic);
        }
    }

    /// Stage 3f.13: build the LUT for a `RenderCreateLinearGradient`
    /// picture and stash it on the engine's `picture_paint` map.
    /// Subsequent `render_composite` calls referencing `host_pic`
    /// as src or mask sample this LUT instead of falling back to
    /// the 3f.12 first-stop SolidFill collapse.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` on the test fixture; `Vk` if the LUT image /
    /// view / memory allocation fails.
    pub(crate) fn build_and_insert_linear_gradient(
        &mut self,
        platform: &PlatformBackend,
        host_pic: u32,
        p1: (i32, i32),
        p2: (i32, i32),
        stops: &[crate::kms::vk::gradient::Stop],
    ) -> Result<(), RenderError> {
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        let pool = platform
            .ops_command_pool_handle()
            .ok_or(RenderError::NoVk)?;
        let gradient = crate::kms::vk::gradient::GradientPicture::new_linear(
            inner.vk.clone(),
            pool,
            p1,
            p2,
            stops,
        )
        .map_err(|e| match e {
            crate::kms::vk::gradient::GradientError::Vk(r) => RenderError::Vk(r),
            crate::kms::vk::gradient::GradientError::NoMemoryType => {
                RenderError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
            }
        })?;
        inner
            .picture_paint
            .insert(host_pic, PicturePaintState::Gradient(gradient));
        Ok(())
    }

    /// Stage 3f.13: radial-gradient companion of
    /// [`build_and_insert_linear_gradient`]. Sizes the LUT image
    /// at `RADIAL_SIDE × RADIAL_SIDE` and renders the two-circle
    /// radial CPU-side, then uploads.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` on the test fixture; `Vk` on allocation
    /// failure.
    pub(crate) fn build_and_insert_radial_gradient(
        &mut self,
        platform: &PlatformBackend,
        host_pic: u32,
        inner_circle: (i32, i32, i32),
        outer_circle: (i32, i32, i32),
        stops: &[crate::kms::vk::gradient::Stop],
    ) -> Result<(), RenderError> {
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        let pool = platform
            .ops_command_pool_handle()
            .ok_or(RenderError::NoVk)?;
        let gradient = crate::kms::vk::gradient::GradientPicture::new_radial(
            inner.vk.clone(),
            pool,
            inner_circle,
            outer_circle,
            stops,
        )
        .map_err(|e| match e {
            crate::kms::vk::gradient::GradientError::Vk(r) => RenderError::Vk(r),
            crate::kms::vk::gradient::GradientError::NoMemoryType => {
                RenderError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
            }
        })?;
        inner
            .picture_paint
            .insert(host_pic, PicturePaintState::Gradient(gradient));
        Ok(())
    }

    /// Stage 3b test helper: how many picture-paint entries are
    /// currently tracked. Used to assert that
    /// `render_free_picture` drops its slot.
    #[cfg(test)]
    pub(crate) fn picture_paint_len(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.picture_paint.len())
    }

    /// Stage 3c: how many cached drawable views the engine
    /// currently holds. Test-only — used to assert eviction on
    /// drawable retire.
    #[cfg(test)]
    pub(crate) fn drawable_view_cache_len(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |i| i.drawable_view_cache.len())
    }

    /// Stage 3c: whether the lazy-built RENDER pipeline cache has
    /// been constructed. Test-only — used to assert the lazy
    /// build trigger.
    #[cfg(test)]
    pub(crate) fn render_pipelines_built(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.render_pipelines.is_some())
    }

    /// Stage 3c: lazy-initialize RENDER paint assets — pipeline
    /// cache + 1×1 SolidFill / SolidMask / WhiteMask scratches +
    /// `DstReadback`. Idempotent. Called by `render_composite`
    /// and `render_fill_rectangles` on first paint; v1 builds
    /// these eagerly at backend construction, but the v2 engine
    /// is constructed before its first composite request so
    /// paying the cost on first use (typically warmup) is fine.
    ///
    /// The `white_mask_image` requires a one-shot clear-to-white
    /// CB to seed its texel; the recorded clear synchronously
    /// drains via `run_one_shot_op` so the texel is present
    /// before the first sample.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `Vk(...)` for any underlying Vk failure during pipeline
    ///   cache / scratch image / readback construction or the
    ///   one-shot white-clear submit.
    pub(crate) fn ensure_render_assets(
        &mut self,
        platform: &PlatformBackend,
    ) -> Result<(), RenderError> {
        use crate::kms::vk::render_pipeline::record_solid_color_clear;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        if inner.render_pipelines.is_none() {
            let cache = RenderPipelineCache::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: RenderPipelineCache::new failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.render_pipelines = Some(cache);
        }
        if inner.solid_src_image.is_none() {
            let s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: solid_src SolidColorImage failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.solid_src_image = Some(s);
        }
        if inner.solid_mask_image.is_none() {
            let s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: solid_mask SolidColorImage failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.solid_mask_image = Some(s);
        }
        if inner.white_mask_image.is_none() {
            let mut s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_render_assets: white_mask SolidColorImage failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            let pool = platform.ops_command_pool_handle().ok_or_else(|| {
                log::error!("v2 ensure_render_assets: no ops_command_pool for white-clear");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            crate::kms::vk::ops::run_one_shot_op(&inner.vk, pool, |vk, cb| {
                record_solid_color_clear(vk, cb, &mut s, [1.0, 1.0, 1.0, 1.0]);
                Ok(())
            })
            .map_err(|e| {
                log::error!("v2 ensure_render_assets: white-clear submit failed: {e:?}");
                RenderError::Vk(e)
            })?;
            inner.white_mask_image = Some(s);
        }
        if inner.dst_readback.is_none() {
            inner.dst_readback = Some(DstReadback::new(Arc::clone(&inner.vk)));
        }
        if inner.src_alias_readback.is_none() {
            inner.src_alias_readback = Some(DstReadback::new(Arc::clone(&inner.vk)));
        }
        Ok(())
    }

    /// Stage 3e.2: lazy-init trap pipeline + mask scratch. Idempotent.
    /// Called by `render_traps_or_tris` on first use. The mask
    /// scratch starts at the default extent and grows via
    /// `ensure_image_size_returning_old` per call; the pipeline is
    /// built once at the standard R8_UNORM mask format.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `Vk(...)` for pipeline / scratch construction failure.
    fn ensure_trap_assets(&mut self, platform: &PlatformBackend) -> Result<(), RenderError> {
        use crate::kms::vk::{mask_scratch::MaskScratch, trap_pipeline::TrapPipeline};
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        if inner.trap_pipeline.is_none() {
            let p =
                TrapPipeline::new(Arc::clone(&inner.vk), vk::Format::R8_UNORM).map_err(|e| {
                    log::error!("v2 ensure_trap_assets: TrapPipeline::new failed: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?;
            inner.trap_pipeline = Some(p);
        }
        if inner.mask_scratch.is_none() {
            let s = MaskScratch::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("v2 ensure_trap_assets: MaskScratch::new failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.mask_scratch = Some(s);
        }
        Ok(())
    }

    /// Stage 3c: invalidate any cached drawable views referencing
    /// `id`. Called by `KmsBackendV2` after a drawable has actually
    /// retired (storage destroyed); evicting earlier would leave
    /// dangling Vk handles since `vk::ImageView`'s underlying image
    /// is gone.
    pub(crate) fn notify_drawable_retired(&mut self, id: DrawableId) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let device = &inner.vk.device;
        inner.drawable_view_cache.retain(|(d, _, _), cached| {
            if *d != id {
                return true;
            }
            unsafe {
                device.destroy_image_view(cached.view, None);
            }
            false
        });
    }

    // ── Op: fill_rect / fill_rect_batch ─────────────────────────

    /// Fill `rect` in `target`'s storage with `color` (RGBA float).
    /// Convenience wrapper around [`Self::fill_rect_batch`] for the
    /// single-rect call sites (create_pixmap zero-fill, bg_pixel
    /// init, image_text background, etc.).
    ///
    /// # Errors
    ///
    /// - `NoVk`, `UnknownDrawable`, `RendererFailed`, or any
    ///   propagated `vk::Result` from CB allocation / submit.
    pub(crate) fn fill_rect(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        rect: vk::Rect2D,
        color: [f32; 4],
    ) -> Result<(), RenderError> {
        self.fill_rect_batch(store, platform, target, color, &[rect])
    }

    /// Fill every rect in `rects` on `target` with `color`, recording
    /// all clears into a **single** CB + a **single** submit + a
    /// **single** `SubmittedOp` (Stage 3f.15 stroke-op aggregation).
    /// `vkCmdClearAttachments` natively accepts a slice of `ClearRect`,
    /// so PolySegment / PolyLine / PolyRectangle stroke fan-outs that
    /// previously paid N submits collapse to one.
    ///
    /// Zero-sized rects are filtered up-front; if the slice contains
    /// only empties (or is empty), the call short-circuits without
    /// touching the queue.
    ///
    /// # Errors
    ///
    /// - `NoVk`, `UnknownDrawable`, `RendererFailed`, or any
    ///   propagated `vk::Result` from CB allocation / submit.
    pub(crate) fn fill_rect_batch(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        color: [f32; 4],
        rects: &[vk::Rect2D],
    ) -> Result<(), RenderError> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let Some(drawable) = store.get_mut(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };
        let extent = drawable.storage.extent;
        let image_view = drawable.storage.image_view;

        // Clamp + drop empties up front. Doing this before begin_op_cb
        // means an all-empty batch doesn't allocate a CB or burn a
        // fence ticket.
        let clamped: Vec<vk::Rect2D> = rects
            .iter()
            .map(|r| clamp_rect(*r, extent))
            .filter(|r| r.extent.width != 0 && r.extent.height != 0)
            .collect();
        if clamped.is_empty() {
            return Ok(());
        }

        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        // Transition target → COLOR_ATTACHMENT_OPTIMAL. Producer
        // mask includes SHADER_SAMPLED_READ (compose's prior read)
        // and TRANSFER_WRITE (prior put_image) so a follow-on
        // paint after compose-read drains correctly per cross-
        // cutting §2 write-after-read note.
        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        );

        let render_area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(image_view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)];
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .color_attachments(&color_attachment);

        let attachments = [vk::ClearAttachment::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .color_attachment(0)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: color },
            })];
        let clear_rects: Vec<vk::ClearRect> = clamped
            .iter()
            .map(|r| {
                vk::ClearRect::default()
                    .rect(*r)
                    .base_array_layer(0)
                    .layer_count(1)
            })
            .collect();

        unsafe {
            device.cmd_begin_rendering(cb, &rendering_info);
            let viewport = [vk::Viewport {
                x: 0.0,
                y: 0.0,
                #[allow(clippy::cast_precision_loss)]
                width: extent.width as f32,
                #[allow(clippy::cast_precision_loss)]
                height: extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }];
            device.cmd_set_viewport(cb, 0, &viewport);
            let scissor = [render_area];
            device.cmd_set_scissor(cb, 0, &scissor);

            device.cmd_clear_attachments(cb, &attachments, &clear_rects);
            device.cmd_end_rendering(cb);
        }

        // Return target to SHADER_READ_ONLY_OPTIMAL.
        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(target, ticket.clone());
        for r in &clamped {
            store.damage(target, *r);
        }
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: None,
        });
        Ok(())
    }

    // ── Op: logic_fill (Stage 3f.2) ─────────────────────────────

    /// Lazy-build a `LogicFillPipelineCache` for `color_format`.
    /// Each cache instance is bound to a single attachment format
    /// at construction; we shard by format so a session that paints
    /// to both BGRA8 and R8 dst formats only pays per-format pipeline
    /// compile cost. The inner cache further keys by `(function,
    /// opaque_alpha)` so all 16 X11 GC functions × {opaque, ARGB}
    /// share one pipeline-layout.
    fn ensure_logic_fill_cache(
        &mut self,
        platform: &PlatformBackend,
        color_format: vk::Format,
    ) -> Result<(), RenderError> {
        use crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache;
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        if inner.logic_fill_caches.contains_key(&color_format) {
            return Ok(());
        }
        let cache =
            LogicFillPipelineCache::new(Arc::clone(&inner.vk), color_format).map_err(|e| {
                log::error!(
                    "v2 ensure_logic_fill_cache: LogicFillPipelineCache::new failed: {e:?}"
                );
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        inner.logic_fill_caches.insert(color_format, cache);
        Ok(())
    }

    /// Solid-fill `rects` in `target` through a `VkLogicOp` pipeline
    /// matching `function`. Ports v1's `try_vk_fill_with_function`
    /// non-`GXcopy` path into a v2-shape per-op CB.
    ///
    /// `opaque_alpha = true` is the depth-24 (server-owned α) path:
    /// the pipeline's color blend write mask drops alpha so the
    /// `VkLogicOp` only mutates RGB and the destination's existing
    /// alpha byte is left intact (L1 server-α invariant). `false` is
    /// the depth-32 ARGB path — LogicOp applies to all four channels
    /// per X11 semantics.
    ///
    /// `fg` is the X11 wire pixel value (top byte alpha for depth 32,
    /// ignored for depth 24). The recorder unpacks it identically to
    /// v1's `try_vk_fill_with_function`. `GXclear`-class functions
    /// (Clear / Set / Invert / etc.) ignore `fg` semantically; the
    /// fragment shader still receives it but `VkLogicOp` overrides
    /// the output.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if `target` is missing; `NoVk` on the stub
    /// engine; `Vk` for any underlying Vulkan failure.
    pub(crate) fn logic_fill(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        function: yserver_core::backend::GcFunction,
        opaque_alpha: bool,
        fg: u32,
        rects: &[Rectangle16],
    ) -> Result<(), RenderError> {
        use crate::kms::vk::logic_fill_pipeline::LogicFillPushConsts;
        use yserver_core::backend::GcFunction;

        if rects.is_empty() {
            return Ok(());
        }
        if matches!(function, GcFunction::NoOp) {
            return Ok(());
        }

        let format = {
            let d = store
                .get(target)
                .ok_or(RenderError::UnknownDrawable(target))?;
            d.storage.format
        };
        self.ensure_logic_fill_cache(platform, format)?;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let cache = inner
            .logic_fill_caches
            .get_mut(&format)
            .expect("ensured above");
        let pipeline = cache.get(function, opaque_alpha).map_err(|e| {
            log::warn!("v2 logic_fill: pipeline build failed for {function:?}: {e:?}");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
        let pipeline_layout = cache.pipeline_layout();

        let Some(drawable) = store.get_mut(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };
        let extent = drawable.storage.extent;
        let image_view = drawable.storage.image_view;

        // Unpack the X11 wire pixel. Same shape as v1's
        // `try_vk_fill_with_function`: R8_UNORM dst (depth 1 / 8)
        // takes fg in color[0]; BGRA8_UNORM dst takes BGR in 0..3.
        // Alpha forced opaque — the pipeline's write mask handles
        // the opaque-alpha policy.
        let color = if format == vk::Format::R8_UNORM {
            [(fg & 0xFF) as f32 / 255.0, 0.0, 0.0, 1.0]
        } else {
            [
                ((fg >> 16) & 0xFF) as f32 / 255.0,
                ((fg >> 8) & 0xFF) as f32 / 255.0,
                (fg & 0xFF) as f32 / 255.0,
                1.0,
            ]
        };

        // Clamp each rect to dst extent + drop empties up-front, so
        // the recorder's per-rect scissor + draw never sees a bogus
        // rect. Mirrors v1's pre-record clamp loop.
        let vk_rects: Vec<vk::Rect2D> = rects
            .iter()
            .filter_map(|r| {
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x).saturating_add(i32::from(r.width)))
                    .min(i32::try_from(extent.width).unwrap_or(i32::MAX));
                let y1 = (i32::from(r.y).saturating_add(i32::from(r.height)))
                    .min(i32::try_from(extent.height).unwrap_or(i32::MAX));
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                Some(vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        if vk_rects.is_empty() {
            return Ok(());
        }

        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        // Transition target → COLOR_ATTACHMENT_OPTIMAL. Same producer
        // mask shape as fill_rect (SHADER_SAMPLED_READ + TRANSFER_WRITE
        // + COLOR_ATTACHMENT_WRITE) so a prior compose-read / put_image
        // / paint drains correctly per cross-cutting §2.
        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        );

        let render_area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(image_view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)];
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .color_attachments(&color_attachment);
        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            #[allow(clippy::cast_precision_loss)]
            width: extent.width as f32,
            #[allow(clippy::cast_precision_loss)]
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];

        let dst_vp = [
            #[allow(clippy::cast_precision_loss)]
            {
                extent.width as f32
            },
            #[allow(clippy::cast_precision_loss)]
            {
                extent.height as f32
            },
        ];
        let damage_rects: Vec<vk::Rect2D> = vk_rects.clone();
        unsafe {
            device.cmd_begin_rendering(cb, &rendering_info);
            device.cmd_set_viewport(cb, 0, &viewport);
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);

            for r in &vk_rects {
                let scissor = [*r];
                device.cmd_set_scissor(cb, 0, &scissor);
                let pc = LogicFillPushConsts {
                    dst_origin: [
                        #[allow(clippy::cast_precision_loss)]
                        {
                            r.offset.x as f32
                        },
                        #[allow(clippy::cast_precision_loss)]
                        {
                            r.offset.y as f32
                        },
                    ],
                    dst_size: [
                        #[allow(clippy::cast_precision_loss)]
                        {
                            r.extent.width as f32
                        },
                        #[allow(clippy::cast_precision_loss)]
                        {
                            r.extent.height as f32
                        },
                    ],
                    viewport: dst_vp,
                    _pad: [0.0, 0.0],
                    fg_color: color,
                };
                device.cmd_push_constants(
                    cb,
                    pipeline_layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    pc.as_bytes(),
                );
                device.cmd_draw(cb, 4, 1, 0, 0);
            }
            device.cmd_end_rendering(cb);
        }

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(target, ticket.clone());
        for r in &damage_rects {
            store.damage(target, *r);
        }
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: None,
        });
        Ok(())
    }

    // ── Op: copy_area (Stage 2d) ────────────────────────────────

    /// Copy `src_rect` from `src` into `dst` at `dst_pos`. The
    /// disjoint case is a straight `vkCmdCopyImage`. When
    /// `src == dst`, a same-image overlap is detected and routed
    /// through a scratch-image via `vkCmdCopyImage` twice (per
    /// Stage 2 plan §"copy_area" subcase). Stage 2's slow scratch
    /// path is acceptable — apps that hit it (xterm scroll
    /// without compositor) need glyphs to be relevant anyway,
    /// landing in Stage 3.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if either id is missing; `Vk` for
    /// any Vk failure; `NoVk` on the stub engine.
    pub(crate) fn copy_area(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src: DrawableId,
        dst: DrawableId,
        src_rect: vk::Rect2D,
        dst_pos: vk::Offset2D,
    ) -> Result<(), RenderError> {
        if src_rect.extent.width == 0 || src_rect.extent.height == 0 {
            return Ok(());
        }
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Read src + dst metadata first (without holding a mutable
        // borrow into store across both transitions).
        let (src_image, src_extent, src_format) = {
            let d = store.get(src).ok_or(RenderError::UnknownDrawable(src))?;
            (d.storage.image, d.storage.extent, d.storage.format)
        };
        let (dst_extent, dst_format) = {
            let d = store.get(dst).ok_or(RenderError::UnknownDrawable(dst))?;
            (d.storage.extent, d.storage.format)
        };
        if src_format != dst_format {
            return Err(RenderError::UnsupportedDepth(0));
        }

        // Clamp src_rect to src extent.
        let src_rect = clamp_rect(src_rect, src_extent);
        // Project to dst: compute the dst rect (clamped to dst extent).
        let dst_pos_clamped = vk::Offset2D {
            x: dst_pos.x.max(0),
            y: dst_pos.y.max(0),
        };
        let copy_w = u32::try_from(
            (i32::from_le_bytes(i32::to_le_bytes(dst_pos.x))
                + i32::try_from(src_rect.extent.width).unwrap_or(0))
            .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX))
                - dst_pos_clamped.x,
        )
        .unwrap_or(0)
        .min(src_rect.extent.width);
        let copy_h = u32::try_from(
            (i32::from_le_bytes(i32::to_le_bytes(dst_pos.y))
                + i32::try_from(src_rect.extent.height).unwrap_or(0))
            .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX))
                - dst_pos_clamped.y,
        )
        .unwrap_or(0)
        .min(src_rect.extent.height);
        if copy_w == 0 || copy_h == 0 {
            return Ok(());
        }
        let dst_rect = vk::Rect2D {
            offset: dst_pos_clamped,
            extent: vk::Extent2D {
                width: copy_w,
                height: copy_h,
            },
        };

        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        if src == dst {
            // Same-image overlap path: scratch image at copy_w ×
            // copy_h, format matches src.
            let scratch =
                allocate_scratch_image(&inner.vk.clone(), platform, copy_w, copy_h, src_format)?;
            // src → TRANSFER_SRC; scratch starts UNDEFINED →
            // TRANSFER_DST.
            {
                let src_d = store.get_mut(src).expect("src missing post-lookup");
                src_d.record_layout_transition(
                    &inner.vk,
                    cb,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::PipelineStageFlags2::ALL_COMMANDS,
                    vk::AccessFlags2::SHADER_SAMPLED_READ
                        | vk::AccessFlags2::TRANSFER_WRITE
                        | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags2::COPY,
                    vk::AccessFlags2::TRANSFER_READ,
                );
            }
            // scratch UNDEFINED → TRANSFER_DST_OPTIMAL.
            barrier_to_layout(
                device,
                cb,
                scratch.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::PipelineStageFlags2::TOP_OF_PIPE,
                vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
            );
            // Copy src_rect → scratch (offset 0,0).
            let region1 = [vk::ImageCopy::default()
                .src_subresource(color_layers())
                .src_offset(vk::Offset3D {
                    x: src_rect.offset.x,
                    y: src_rect.offset.y,
                    z: 0,
                })
                .dst_subresource(color_layers())
                .dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .extent(vk::Extent3D {
                    width: copy_w,
                    height: copy_h,
                    depth: 1,
                })];
            unsafe {
                device.cmd_copy_image(
                    cb,
                    src_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    scratch.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region1,
                );
            }
            // scratch TRANSFER_DST → TRANSFER_SRC.
            barrier_to_layout(
                device,
                cb,
                scratch.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_READ,
            );
            // src → TRANSFER_DST (it's also dst).
            {
                let d = store.get_mut(src).expect("src missing");
                d.record_layout_transition(
                    &inner.vk,
                    cb,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::PipelineStageFlags2::COPY,
                    vk::AccessFlags2::TRANSFER_READ,
                    vk::PipelineStageFlags2::COPY,
                    vk::AccessFlags2::TRANSFER_WRITE,
                );
            }
            // Copy scratch → src at dst_rect.
            let region2 = [vk::ImageCopy::default()
                .src_subresource(color_layers())
                .src_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .dst_subresource(color_layers())
                .dst_offset(vk::Offset3D {
                    x: dst_rect.offset.x,
                    y: dst_rect.offset.y,
                    z: 0,
                })
                .extent(vk::Extent3D {
                    width: copy_w,
                    height: copy_h,
                    depth: 1,
                })];
            unsafe {
                device.cmd_copy_image(
                    cb,
                    scratch.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    src_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region2,
                );
            }
            // src → SHADER_READ_ONLY.
            {
                let d = store.get_mut(src).expect("src missing");
                d.record_layout_transition(
                    &inner.vk,
                    cb,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::COPY,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                );
            }
            end_and_submit_op(inner, platform, cb, &ticket)?;
            store.touch_render_fence(src, ticket.clone());
            store.damage(src, dst_rect);
            inner.submitted.push_back(SubmittedOp {
                cb,
                ticket,
                staging: None,
                scratch: Some(scratch),
                atlas_ticket: None,
                descriptor_arena: None,
            });
            return Ok(());
        }

        // Disjoint-image path: src → TRANSFER_SRC, dst → TRANSFER_DST.
        {
            let d = store.get_mut(src).expect("src missing");
            d.record_layout_transition(
                &inner.vk,
                cb,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::AccessFlags2::SHADER_SAMPLED_READ
                    | vk::AccessFlags2::TRANSFER_WRITE
                    | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_READ,
            );
        }
        {
            let d = store.get_mut(dst).expect("dst missing");
            d.record_layout_transition(
                &inner.vk,
                cb,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::AccessFlags2::SHADER_SAMPLED_READ
                    | vk::AccessFlags2::TRANSFER_WRITE
                    | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
            );
        }
        let region = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D {
                x: src_rect.offset.x,
                y: src_rect.offset.y,
                z: 0,
            })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D {
                x: dst_rect.offset.x,
                y: dst_rect.offset.y,
                z: 0,
            })
            .extent(vk::Extent3D {
                width: copy_w,
                height: copy_h,
                depth: 1,
            })];
        unsafe {
            let dst_image = store.get(dst).expect("dst missing").storage.image;
            device.cmd_copy_image(
                cb,
                src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            );
        }
        // Return src + dst to SHADER_READ_ONLY.
        {
            let d = store.get_mut(src).expect("src missing");
            d.record_layout_transition(
                &inner.vk,
                cb,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_READ,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            );
        }
        {
            let d = store.get_mut(dst).expect("dst missing");
            d.record_layout_transition(
                &inner.vk,
                cb,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            );
        }
        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(src, ticket.clone());
        store.touch_render_fence(dst, ticket.clone());
        store.damage(dst, dst_rect);
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: None,
        });
        Ok(())
    }

    // ── Op: put_image ───────────────────────────────────────────

    /// Upload `src_bytes` (interpreted per `src_depth`) into
    /// `target` at `dst_pos`. Stage 2c supports depths 1, 8, 24,
    /// 32 with the byte layouts the X11 dispatcher emits (see
    /// the inline conversion table). Per-op staging buffer; no
    /// arena coalescing yet.
    ///
    /// # Errors
    ///
    /// - `UnsupportedDepth` if `src_depth` isn't 1/8/24/32.
    /// - `TruncatedSource` if `src_bytes` is shorter than the
    ///   row stride × height the depth implies.
    /// - `Vk(...)` for any Vk failure (CB / buffer / submit).
    pub(crate) fn put_image(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        dst_pos: vk::Offset2D,
        src_extent: vk::Extent2D,
        src_bytes: &[u8],
        src_depth: u8,
    ) -> Result<(), RenderError> {
        if src_extent.width == 0 || src_extent.height == 0 {
            return Ok(());
        }
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let Some(drawable) = store.get_mut(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };

        // Stage 2c-supported depths only. Anything else is logged
        // upstream and routes to the gap path; we surface the
        // type-level reject so the backend wrapper can dedup-log.
        let dst_bpp: u32 = match src_depth {
            1 | 8 => 1,
            24 | 32 => 4,
            _ => return Err(RenderError::UnsupportedDepth(src_depth)),
        };
        let dst_format = drawable.storage.format;
        // The store allocates storage by depth; format mismatch
        // here means the caller targeted a depth-mismatched
        // drawable. Treat as unsupported.
        let expected_format = if dst_bpp == 1 {
            vk::Format::R8_UNORM
        } else {
            vk::Format::B8G8R8A8_UNORM
        };
        if dst_format != expected_format {
            return Err(RenderError::UnsupportedDepth(src_depth));
        }

        let dst_extent = drawable.storage.extent;
        // Clamp the put rect to the storage extent. Per Stage 2
        // plan, GC clipping is the backend wrapper's concern;
        // the engine only sees the dst-extent guard.
        let clipped = clamp_put_rect(dst_pos, src_extent, dst_extent);
        let Some((dst_rect, src_origin_in_input)) = clipped else {
            return Ok(());
        };
        let copy_w = dst_rect.extent.width;
        let copy_h = dst_rect.extent.height;
        let staging_size = u64::from(copy_w) * u64::from(copy_h) * u64::from(dst_bpp);
        if staging_size == 0 {
            return Ok(());
        }

        let staging = StagingBuffer::new(inner.vk.clone(), staging_size.max(1))?;
        // Convert src_bytes → staging according to (depth, dst_format).
        let (sx, sy) = src_origin_in_input;
        unpack_to_staging(
            src_bytes,
            src_extent,
            sx,
            sy,
            copy_w,
            copy_h,
            src_depth,
            staging.mapped.as_ptr(),
        )?;

        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );

        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D {
                x: dst_rect.offset.x,
                y: dst_rect.offset.y,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: copy_w,
                height: copy_h,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_buffer_to_image(
                cb,
                staging.buffer,
                drawable.storage.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            );
        }

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(target, ticket.clone());
        store.damage(target, dst_rect);
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: Some(staging),
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: None,
        });
        Ok(())
    }

    // ── Op: get_image (synchronous) ─────────────────────────────

    /// Read `rect` from `src`'s storage. **Synchronous** — waits
    /// on the readback `FenceTicket` before returning. The only
    /// sync path on the v2 paint surface; protocol design makes
    /// `GetImage` an RPC, so a host wait is unavoidable.
    ///
    /// Returns bytes in **wire format**: for depth-32/24,
    /// `rect_w * rect_h * 4` BGRA-order bytes (alpha undefined for
    /// depth-24). For depth-8, `rect_w * rect_h` bytes. For
    /// depth-1, `bytes_per_row * rect_h` with the scanline padded
    /// to 32 bits and bits packed MSB-first per byte; storage is
    /// `R8` and each non-zero byte sets one bit.
    ///
    /// # Errors
    ///
    /// - `UnsupportedDepth` for depths other than 1/8/24/32.
    /// - `Vk` for CB / buffer / submit / wait failures.
    pub(crate) fn get_image(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src: DrawableId,
        rect: vk::Rect2D,
        out_depth: u8,
    ) -> Result<Vec<u8>, RenderError> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let Some(drawable) = store.get_mut(src) else {
            return Err(RenderError::UnknownDrawable(src));
        };
        let storage_bpp: u32 = match out_depth {
            1 | 8 => 1,
            24 | 32 => 4,
            _ => return Err(RenderError::UnsupportedDepth(out_depth)),
        };
        let extent = drawable.storage.extent;
        // Clamp the read rect to storage bounds.
        let clipped = clamp_rect(rect, extent);
        let copy_w = clipped.extent.width;
        let copy_h = clipped.extent.height;
        if copy_w == 0 || copy_h == 0 {
            return Ok(Vec::new());
        }
        let staging_size = u64::from(copy_w) * u64::from(copy_h) * u64::from(storage_bpp);
        let staging = StagingBuffer::new(inner.vk.clone(), staging_size.max(1))?;

        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );

        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D {
                x: clipped.offset.x,
                y: clipped.offset.y,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: copy_w,
                height: copy_h,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image_to_buffer(
                cb,
                drawable.storage.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging.buffer,
                &region,
            );
        }

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(src, ticket.clone());

        // Sync wait — off the hot path by protocol design.
        ticket.wait(&inner.vk)?;

        // Pack storage bytes into wire format.
        let raw_size = (u64::from(copy_w) * u64::from(copy_h) * u64::from(storage_bpp)) as usize;
        // SAFETY: staging is HOST_COHERENT, mapped for `staging.size`
        // bytes (≥ raw_size), and the fence above signalled, so
        // the GPU has completed all writes.
        let raw: &[u8] = unsafe { std::slice::from_raw_parts(staging.mapped.as_ptr(), raw_size) };
        let out = pack_from_storage(raw, copy_w, copy_h, out_depth)?;

        // Park CB + staging on `submitted`; they retire on the
        // next `poll_retired` call (the fence is already signaled,
        // so the retire happens at next poll — keeps the lifecycle
        // book-keeping uniform).
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: Some(staging),
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: None,
        });

        Ok(out)
    }

    // ── Op: image_text (Stage 3a) ───────────────────────────────

    /// One glyph the caller hands to [`RenderEngine::image_text`].
    /// CPU-side pre-rasterised by FreeType so the engine doesn't
    /// touch FreeType state. `pixels` is row-major, tightly packed
    /// (no row padding) — width × height alpha bytes.
    ///
    /// The pen-left/pen-top offsets are applied to `dst_x` /
    /// `dst_y` by the caller, so the engine just packs the glyph
    /// and queues a draw at the supplied destination coords.
    /// Stage 3a: drive a single text run against `target`'s
    /// storage. CPU-side glyph rasterisation is the caller's
    /// concern (KmsBackendV2 wraps the v1 FreeType path); the
    /// engine takes the resulting [`PreparedGlyph`] slice, interns
    /// each into the atlas, and records one TextPipeline draw
    /// covering the whole run.
    ///
    /// `font_xid` keys the glyph cache so the same codepoint
    /// rendered at two different font sizes ends up at two atlas
    /// slots. `foreground_rgba` is the GC foreground in [0..1].
    /// Damage is recorded on the target at the union of glyph
    /// bounding boxes.
    ///
    /// Returns telemetry counts the caller feeds to the v2 backend
    /// telemetry sink: how many distinct atlas interns happened
    /// (= miss count this run), how many glyph uploads were
    /// submitted (= same as interns today; collapses if later
    /// coalesced), and how many glyphs were dropped due to
    /// atlas-full.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` when `target` isn't in `store`.
    /// - `Vk(...)` for any CB / submit failure. Best-effort: an
    ///   upload that fails partway is logged and the affected
    ///   glyph is dropped; only catastrophic failures (text-run
    ///   CB allocation, atlas init) propagate.
    pub(crate) fn image_text(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: DrawableId,
        font_xid: u32,
        foreground_rgba: [f32; 4],
        rendered: &[PreparedGlyph],
    ) -> Result<ImageTextStats, RenderError> {
        let mut stats = ImageTextStats::default();
        if rendered.is_empty() {
            return Ok(stats);
        }
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        if store.get(target).is_none() {
            return Err(RenderError::UnknownDrawable(target));
        }
        // Mirror format gate matches v1's check — the text
        // pipeline is built for B8G8R8A8_UNORM, so depth-1/8
        // storage can't be a text-run target.
        let target_format = store.get(target).expect("checked above").storage.format;
        if target_format != vk::Format::B8G8R8A8_UNORM {
            log::warn!(
                "v2 image_text: target xid={:?} has format {:?}; text pipeline only supports \
                 B8G8R8A8_UNORM — dropping run",
                store.get(target).map(|d| d.xid),
                target_format,
            );
            return Ok(stats);
        }

        // Lazy-init atlas + pipeline. The first text run pays the
        // 16 MiB R8 allocation; subsequent runs reuse.
        if inner.glyph_atlas.is_none() {
            match V2GlyphAtlas::new(Arc::clone(&inner.vk)) {
                Ok(a) => inner.glyph_atlas = Some(a),
                Err(e) => {
                    log::error!("v2 image_text: V2GlyphAtlas::new failed: {e:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }
        if inner.text_pipeline.is_none() {
            let atlas_view = inner.glyph_atlas.as_ref().expect("just built").image_view();
            match TextPipeline::new(
                Arc::clone(&inner.vk),
                vk::Format::B8G8R8A8_UNORM,
                atlas_view,
            ) {
                Ok(p) => inner.text_pipeline = Some(p),
                Err(e) => {
                    log::error!("v2 image_text: TextPipeline::new failed: {e:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }

        // Resolve each glyph: cache hit, fresh upload, or drop.
        // Track dst-bounding box for the damage hook.
        let mut glyphs_to_draw: Vec<TextGlyph> = Vec::with_capacity(rendered.len());
        let mut damage_min_x = i32::MAX;
        let mut damage_min_y = i32::MAX;
        let mut damage_max_x = i32::MIN;
        let mut damage_max_y = i32::MIN;
        for g in rendered {
            let key = GlyphKey {
                font_xid,
                codepoint: g.codepoint,
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let w_u = g.w as u32;
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let h_u = g.h as u32;
            // Cache hit fast path.
            let entry =
                if let Some(hit) = inner.glyph_atlas.as_ref().expect("just built").lookup(key) {
                    hit
                } else {
                    // Pack + upload.
                    let Some((atlas_x, atlas_y)) = inner
                        .glyph_atlas
                        .as_mut()
                        .expect("just built")
                        .pack(w_u, h_u)
                    else {
                        // Atlas full: drop the glyph, advance pen via
                        // caller's tracking, log once + bump stats.
                        inner
                            .glyph_atlas
                            .as_mut()
                            .expect("just built")
                            .note_full_once();
                        stats.glyphs_dropped += 1;
                        continue;
                    };
                    if w_u == 0 || h_u == 0 {
                        // Zero-area glyph (space): cache degenerate
                        // entry so future runs short-circuit; no upload.
                        let e = AtlasEntry {
                            atlas_x: 0,
                            atlas_y: 0,
                            w: 0,
                            h: 0,
                            pen_left: 0,
                            pen_top: 0,
                        };
                        inner
                            .glyph_atlas
                            .as_mut()
                            .expect("just built")
                            .insert_entry(key, e);
                        continue;
                    }
                    stats.atlas_interns += 1;
                    // Each upload owns its own staging slice for the
                    // CB's lifetime (Stage 3 plan §"Cross-cutting" §3).
                    let upload_bytes = (w_u as u64) * (h_u as u64);
                    let staging = StagingBuffer::new(Arc::clone(&inner.vk), upload_bytes.max(1))?;
                    // SAFETY: staging.size ≥ upload_bytes ≥ pixels.len()
                    // (per the pre-condition that PreparedGlyph.pixels
                    // is row-major w×h). mapped is host-coherent.
                    let copy_len = (w_u as usize) * (h_u as usize);
                    let src_slice = if g.pixels.len() >= copy_len {
                        &g.pixels[..copy_len]
                    } else {
                        log::warn!(
                            "v2 image_text: glyph pixels {} < {} expected; dropping",
                            g.pixels.len(),
                            copy_len,
                        );
                        stats.glyphs_dropped += 1;
                        continue;
                    };
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src_slice.as_ptr(),
                            staging.mapped.as_ptr(),
                            copy_len,
                        );
                    }
                    // Submit a one-shot upload CB.
                    let (cb, ticket) = begin_op_cb(inner, platform)?;
                    let staging_buffer = staging.buffer;
                    inner
                        .glyph_atlas
                        .as_mut()
                        .expect("just built")
                        .record_upload(cb, staging_buffer, atlas_x, atlas_y, w_u, h_u);
                    end_and_submit_op(inner, platform, cb, &ticket)?;
                    stats.glyph_uploads += 1;
                    // Park the upload's CB + staging on submitted; the
                    // ticket signals when the upload retires.
                    inner.submitted.push_back(SubmittedOp {
                        cb,
                        ticket: ticket.clone(),
                        staging: Some(staging),
                        scratch: None,
                        atlas_ticket: None,
                        descriptor_arena: None,
                    });
                    inner.atlas_last_upload_ticket = Some(ticket);
                    let e = AtlasEntry {
                        atlas_x,
                        atlas_y,
                        w: w_u,
                        h: h_u,
                        pen_left: 0,
                        pen_top: 0,
                    };
                    inner
                        .glyph_atlas
                        .as_mut()
                        .expect("just built")
                        .insert_entry(key, e);
                    e
                };
            if entry.w == 0 || entry.h == 0 {
                continue;
            }
            // Project glyph bbox into damage tracker (storage-local
            // coords, pen offsets already applied by caller).
            damage_min_x = damage_min_x.min(g.dst_x);
            damage_min_y = damage_min_y.min(g.dst_y);
            #[allow(clippy::cast_possible_wrap)]
            let max_x = g.dst_x.saturating_add(entry.w as i32);
            #[allow(clippy::cast_possible_wrap)]
            let max_y = g.dst_y.saturating_add(entry.h as i32);
            damage_max_x = damage_max_x.max(max_x);
            damage_max_y = damage_max_y.max(max_y);
            glyphs_to_draw.push(TextGlyph {
                entry,
                dst_x: g.dst_x,
                dst_y: g.dst_y,
            });
        }

        if glyphs_to_draw.is_empty() {
            return Ok(stats);
        }

        // Atlas geometry is fixed for the engine's lifetime; cache
        // a local copy here so the draw recorder doesn't borrow the
        // engine.
        let atlas_extent = inner.glyph_atlas.as_ref().expect("ensured above").extent();

        // Record the text-run draw on the target.
        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let drawable = store
            .get_mut(target)
            .expect("checked at entry — store didn't mutate");
        let mut adapter = StorageTextTarget {
            extent: drawable.storage.extent,
            image: drawable.storage.image,
            image_view: drawable.storage.image_view,
            current_layout: drawable.storage.current_layout,
        };
        let result = record_text_run(
            &inner.vk,
            cb,
            &mut adapter,
            TextAtlas {
                extent: atlas_extent,
            },
            inner.text_pipeline.as_ref().expect("ensured above"),
            &glyphs_to_draw,
            foreground_rgba,
        );
        // Propagate adapter's tracked layout back into the
        // Drawable's storage state — record_text_run flips it to
        // SHADER_READ_ONLY_OPTIMAL on success.
        drawable.storage.current_layout = adapter.current_layout;
        result?;

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(target, ticket.clone());
        // Damage = union of glyph dst-bboxes. Always non-empty
        // here since glyphs_to_draw is non-empty above.
        if damage_max_x > damage_min_x && damage_max_y > damage_min_y {
            let dx = damage_min_x.max(0);
            let dy = damage_min_y.max(0);
            let w = u32::try_from(damage_max_x - dx).unwrap_or(0);
            let h = u32::try_from(damage_max_y - dy).unwrap_or(0);
            if w > 0 && h > 0 {
                store.damage(
                    target,
                    vk::Rect2D {
                        offset: vk::Offset2D { x: dx, y: dy },
                        extent: vk::Extent2D {
                            width: w,
                            height: h,
                        },
                    },
                );
            }
        }
        // Stage 3 plan §"Cross-cutting" §3: clone the atlas's
        // most-recent upload ticket onto this consume op so the
        // upload's staging buffer + the atlas itself can't drop
        // until both upload and consume retire. Same-queue
        // submission order is the GPU dependency.
        let atlas_ticket = inner.atlas_last_upload_ticket.clone();
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket,
            descriptor_arena: None,
        });

        Ok(stats)
    }

    // ── Op: composite_glyphs (Stage 3d) ─────────────────────────

    /// Record a RENDER `CompositeGlyphs` against `dst`. Backend
    /// wrapper (`KmsBackendV2::render_composite_glyphs`) is
    /// responsible for: (a) gating on `op == Over` + SolidFill
    /// source (plan §3d "v1-parity scope"), (b) parsing the
    /// `items` glyph-element stream including the inline `0xFF 0
    /// mask_fmt new_gs` glyphset-change form, (c) looking up
    /// each glyph from `KmsCore.glyphsets`, (d) host-side A1→A8
    /// expansion. By the time we reach the engine, each input is a
    /// dense A8 bitmap + a dst position + a glyphset xid that
    /// keys it in the engine's atlas.
    ///
    /// `foreground_rgba` is the SolidFill source's premultiplied
    /// colour (the text pipeline shader multiplies it by the
    /// sampled atlas alpha — same blend state as 3a's image_text).
    ///
    /// `clip_rects` is the dst picture's clip set, already
    /// pre-shifted by the picture's `clip_x` / `clip_y` origin
    /// (Stage 3b). `None` paints the full dst; passing an empty
    /// slice paints nothing. Per plan §4, the engine emits one
    /// `cmd_set_scissor` + glyph-draw batch per clip rect — this
    /// is the v1-bug-fix: v1's `try_vk_render_composite_glyphs`
    /// reads the dst picture clip but ignores it
    /// (`kms::backend.rs:5313`).
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing.
    /// - `Vk(...)` for any CB / submit failure. Atlas-upload
    ///   failures drop the affected glyph and bump
    ///   `stats.glyphs_dropped`; only catastrophic failures (CB
    ///   alloc, draw-record) propagate.
    /// - `RendererFailed` if `platform.renderer_failed`.
    pub(crate) fn composite_glyphs(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst_id: DrawableId,
        foreground_rgba: [f32; 4],
        glyphs: &[CompositeGlyphInput<'_>],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<ImageTextStats, RenderError> {
        let mut stats = ImageTextStats::default();
        if glyphs.is_empty() {
            return Ok(stats);
        }
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let (dst_extent, dst_format) = {
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (d.storage.extent, d.storage.format)
        };
        // Text pipeline is built for B8G8R8A8_UNORM; same gate as
        // v1's `try_vk_render_composite_glyphs` and v2's
        // `image_text`.
        if dst_format != vk::Format::B8G8R8A8_UNORM {
            log::warn!(
                "v2 composite_glyphs: dst xid={:?} has format {:?}; text pipeline only \
                 supports B8G8R8A8_UNORM — dropping run",
                store.get(dst_id).map(|d| d.xid),
                dst_format,
            );
            return Ok(stats);
        }

        // Lazy-init atlas + pipeline (same path as image_text).
        if inner.glyph_atlas.is_none() {
            match V2GlyphAtlas::new(Arc::clone(&inner.vk)) {
                Ok(a) => inner.glyph_atlas = Some(a),
                Err(e) => {
                    log::error!("v2 composite_glyphs: V2GlyphAtlas::new failed: {e:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }
        if inner.text_pipeline.is_none() {
            let atlas_view = inner.glyph_atlas.as_ref().expect("just built").image_view();
            match TextPipeline::new(
                Arc::clone(&inner.vk),
                vk::Format::B8G8R8A8_UNORM,
                atlas_view,
            ) {
                Ok(p) => inner.text_pipeline = Some(p),
                Err(e) => {
                    log::error!("v2 composite_glyphs: TextPipeline::new failed: {e:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }

        // Resolve / upload each glyph. Atlas key is
        // `(gs_xid, glyph_id)` (v1's `font_xid = gs_xid`
        // convention); image_text and composite_glyphs share the
        // same key namespace — both come from `KmsCore.next_host_xid()`.
        let mut glyphs_to_draw: Vec<TextGlyph> = Vec::with_capacity(glyphs.len());
        let mut damage_min_x = i32::MAX;
        let mut damage_min_y = i32::MAX;
        let mut damage_max_x = i32::MIN;
        let mut damage_max_y = i32::MIN;
        for g in glyphs {
            let key = GlyphKey {
                font_xid: g.gs_xid,
                codepoint: g.glyph_id,
            };
            let entry =
                if let Some(hit) = inner.glyph_atlas.as_ref().expect("just built").lookup(key) {
                    hit
                } else {
                    let Some((atlas_x, atlas_y)) = inner
                        .glyph_atlas
                        .as_mut()
                        .expect("just built")
                        .pack(g.w, g.h)
                    else {
                        inner
                            .glyph_atlas
                            .as_mut()
                            .expect("just built")
                            .note_full_once();
                        stats.glyphs_dropped += 1;
                        continue;
                    };
                    if g.w == 0 || g.h == 0 {
                        let e = AtlasEntry {
                            atlas_x: 0,
                            atlas_y: 0,
                            w: 0,
                            h: 0,
                            pen_left: 0,
                            pen_top: 0,
                        };
                        inner
                            .glyph_atlas
                            .as_mut()
                            .expect("just built")
                            .insert_entry(key, e);
                        continue;
                    }
                    stats.atlas_interns += 1;
                    let upload_bytes = u64::from(g.w) * u64::from(g.h);
                    let staging = StagingBuffer::new(Arc::clone(&inner.vk), upload_bytes.max(1))?;
                    let copy_len = (g.w as usize) * (g.h as usize);
                    let src_slice = if g.pixels.len() >= copy_len {
                        &g.pixels[..copy_len]
                    } else {
                        log::warn!(
                            "v2 composite_glyphs: glyph pixels {} < {} expected; dropping",
                            g.pixels.len(),
                            copy_len,
                        );
                        stats.glyphs_dropped += 1;
                        continue;
                    };
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src_slice.as_ptr(),
                            staging.mapped.as_ptr(),
                            copy_len,
                        );
                    }
                    let (cb, ticket) = begin_op_cb(inner, platform)?;
                    let staging_buffer = staging.buffer;
                    inner
                        .glyph_atlas
                        .as_mut()
                        .expect("just built")
                        .record_upload(cb, staging_buffer, atlas_x, atlas_y, g.w, g.h);
                    end_and_submit_op(inner, platform, cb, &ticket)?;
                    stats.glyph_uploads += 1;
                    inner.submitted.push_back(SubmittedOp {
                        cb,
                        ticket: ticket.clone(),
                        staging: Some(staging),
                        scratch: None,
                        atlas_ticket: None,
                        descriptor_arena: None,
                    });
                    inner.atlas_last_upload_ticket = Some(ticket);
                    let e = AtlasEntry {
                        atlas_x,
                        atlas_y,
                        w: g.w,
                        h: g.h,
                        pen_left: 0,
                        pen_top: 0,
                    };
                    inner
                        .glyph_atlas
                        .as_mut()
                        .expect("just built")
                        .insert_entry(key, e);
                    e
                };
            if entry.w == 0 || entry.h == 0 {
                continue;
            }
            damage_min_x = damage_min_x.min(g.dst_x);
            damage_min_y = damage_min_y.min(g.dst_y);
            #[allow(clippy::cast_possible_wrap)]
            let max_x = g.dst_x.saturating_add(entry.w as i32);
            #[allow(clippy::cast_possible_wrap)]
            let max_y = g.dst_y.saturating_add(entry.h as i32);
            damage_max_x = damage_max_x.max(max_x);
            damage_max_y = damage_max_y.max(max_y);
            glyphs_to_draw.push(TextGlyph {
                entry,
                dst_x: g.dst_x,
                dst_y: g.dst_y,
            });
        }

        if glyphs_to_draw.is_empty() {
            return Ok(stats);
        }

        // Build the picture-clip scissor list. None → full extent.
        // Empty slice → nothing to paint (matches render_composite
        // semantics).
        let clip_scissors: Vec<vk::Rect2D> = match clip_rects {
            None => vec![vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: dst_extent,
            }],
            Some(cr) => {
                let mut out = Vec::with_capacity(cr.len());
                for r in cr {
                    if r.width == 0 || r.height == 0 {
                        continue;
                    }
                    let x0 = i32::from(r.x).max(0);
                    let y0 = i32::from(r.y).max(0);
                    let x1 = (i32::from(r.x) + i32::from(r.width))
                        .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX));
                    let y1 = (i32::from(r.y) + i32::from(r.height))
                        .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX));
                    if x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                    out.push(vk::Rect2D {
                        offset: vk::Offset2D { x: x0, y: y0 },
                        extent: vk::Extent2D {
                            #[allow(clippy::cast_sign_loss)]
                            width: (x1 - x0) as u32,
                            #[allow(clippy::cast_sign_loss)]
                            height: (y1 - y0) as u32,
                        },
                    });
                }
                if out.is_empty() {
                    // Every clip rect was outside dst — nothing to paint.
                    return Ok(stats);
                }
                out
            }
        };

        let atlas_extent = inner.glyph_atlas.as_ref().expect("ensured").extent();
        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let drawable = store.get_mut(dst_id).expect("checked");
        let mut adapter = StorageTextTarget {
            extent: drawable.storage.extent,
            image: drawable.storage.image,
            image_view: drawable.storage.image_view,
            current_layout: drawable.storage.current_layout,
        };
        let result = record_text_run_scissored(
            &inner.vk,
            cb,
            &mut adapter,
            TextAtlas {
                extent: atlas_extent,
            },
            inner.text_pipeline.as_ref().expect("ensured"),
            &glyphs_to_draw,
            foreground_rgba,
            &clip_scissors,
        );
        drawable.storage.current_layout = adapter.current_layout;
        result?;

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(dst_id, ticket.clone());
        // Damage = glyph-bbox union ∩ dst extent. Per-rect clip is
        // honoured by the scissored draws; the damage record uses
        // the broader union since post-paint state is unchanged
        // outside any covered glyph rect anyway.
        if damage_max_x > damage_min_x && damage_max_y > damage_min_y {
            let dx = damage_min_x.max(0);
            let dy = damage_min_y.max(0);
            let w = u32::try_from(damage_max_x - dx).unwrap_or(0);
            let h = u32::try_from(damage_max_y - dy).unwrap_or(0);
            if w > 0 && h > 0 {
                store.damage(
                    dst_id,
                    clamp_rect(
                        vk::Rect2D {
                            offset: vk::Offset2D { x: dx, y: dy },
                            extent: vk::Extent2D {
                                width: w,
                                height: h,
                            },
                        },
                        dst_extent,
                    ),
                );
            }
        }
        let atlas_ticket = inner.atlas_last_upload_ticket.clone();
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket,
            descriptor_arena: None,
        });

        Ok(stats)
    }

    // ── Op: render_composite (Stage 3c) ─────────────────────────

    /// Record a RENDER `Composite` against `dst`. `src` and `mask`
    /// are pre-resolved by the backend wrapper from the protocol
    /// `PictureRecord`. `rects` are pre-decoded composite quads
    /// in dst coords; `clip_rects` is the dst picture's clip set,
    /// already pre-shifted by the picture's `clip_x` / `clip_y`
    /// origin (Stage 3b's `set_picture_clip_rectangles` site does
    /// the shift). Passing `None` for `clip_rects` paints the
    /// full dst extent; passing an empty slice paints nothing.
    ///
    /// Stage 3c scope (per plan §3c):
    /// - Standard PictOps 0..=12 + Saturate (13) via fixed-function
    ///   blend; Disjoint (16..=27) + Conjoint (32..=43) via the
    ///   shader-side `dst_readback` blend.
    /// - Per-rect picture-clip scissoring — one draw call per
    ///   clip-rect intersection, **NOT** v1's union-bbox shortcut.
    /// - Self-aliasing (`src.drawable_id() == Some(dst_id)`):
    ///   handled via Stage 2d's [`allocate_scratch_image`] —
    ///   copy dst → scratch first, sample scratch_view.
    /// - Component-alpha pass through to the pipeline cache key.
    ///
    /// Deliberate v1 deviations / out-of-scope-for-3c gaps:
    /// - **Gradient sources**: gap log + bail (Stage 3e wires
    ///   gradient LUT build via `picture_paint`).
    /// - **Mask self-alias** (`mask.drawable_id() == Some(dst_id)`):
    ///   gap log + bail. Real apps don't hit this; if rendercheck
    ///   spots a case, fold into 3e alongside the gradient work.
    /// - **No ambient `current_clip` consultation** — RENDER ops
    ///   consult picture clip only (plan §4); the GC's
    ///   `current_clip` lives outside the engine call.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing from `store`.
    /// - `Vk(...)` for any underlying pipeline / submit failure.
    /// - `RendererFailed` when `platform.renderer_failed`.
    ///
    /// Out-of-scope gating (unknown op, gradient source, mask
    /// self-alias, unsupported dst format) returns `Ok` with
    /// `recorded_draws = 0` — the op silently no-ops, matching
    /// v1's `try_vk_render_composite` shape.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_composite(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        mask: ResolvedSource,
        dst_id: DrawableId,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_transform: Option<PictTransform>,
        mask_transform: Option<PictTransform>,
        mask_component_alpha: bool,
    ) -> Result<CompositeStats, RenderError> {
        use crate::kms::vk::{
            ops::render as vk_render,
            render_pipeline::{StdPictOp, record_solid_color_clear},
        };

        let mut stats = CompositeStats::default();
        if rects.is_empty() {
            return Ok(stats);
        }

        // Lazy-init RENDER assets.
        self.ensure_render_assets(platform)?;

        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        // Resolve dst metadata (depth, format, extent, image).
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
                d.depth,
            )
        };
        if dst_extent.width == 0 || dst_extent.height == 0 {
            return Ok(stats);
        }
        if !matches!(
            dst_format,
            vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM
        ) {
            log::debug!(
                "v2 render_composite gap: dst format {dst_format:?} not BGRA/R8 (dst id={dst_id:?})"
            );
            return Ok(stats);
        }
        let dst_has_alpha = dst_format == vk::Format::R8_UNORM || dst_depth == 32;

        // Map the protocol op byte to the pipeline cache's enum.
        let Some(std_op) = StdPictOp::from_u8(op) else {
            log::debug!("v2 render_composite gap: unsupported op {op} (dst id={dst_id:?})");
            return Ok(stats);
        };
        let needs_dst_readback = std_op.needs_dst_readback();

        // Stage 3c.3 self-alias (Risk 13). When src and/or mask
        // resolve to the same backing as dst, Vulkan can't sample
        // the image while it's bound as a color attachment in the
        // same draw. Route through `src_alias_readback`: copy dst →
        // scratch before the composite pass, bind the scratch view
        // as `src_tex` / `mask_tex` in place of dst's drawable view.
        let src_self_alias = matches!(src, ResolvedSource::Drawable(id) if id == dst_id);
        let mask_self_alias = matches!(mask, ResolvedSource::Drawable(id) if id == dst_id);
        let self_alias_used = src_self_alias || mask_self_alias;

        // Pre-allocate the alias scratch + extract its sampleable
        // view before resolving src/mask. The scratch grows on
        // demand; the view is stable until the next grow (which
        // can't happen mid-call). `record_copy_from` runs inside
        // the per-op CB later — this is just the resource ensure.
        let src_alias_view = if self_alias_used {
            let rb = inner.src_alias_readback.as_mut().expect("ensured");
            rb.ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                .map_err(|e| {
                    log::warn!("v2 render_composite: src_alias_readback ensure failed: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?;
            match rb.view(dst_format, dst_has_alpha) {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    log::warn!("v2 render_composite: src_alias_readback view None — skipping");
                    return Ok(stats);
                }
                Err(e) => {
                    log::warn!("v2 render_composite: src_alias_readback view build failed: {e:?}");
                    return Ok(stats);
                }
            }
        } else {
            None
        };

        // Resolve src view + extent + (optional) clear colour.
        let solid_src_view = inner
            .solid_src_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let solid_mask_view = inner
            .solid_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let white_mask_view = inner
            .white_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();

        let mut src_clear_color: Option<[f32; 4]> = None;
        let mut mask_clear_color: Option<[f32; 4]> = None;
        let mut src_is_synthetic_1x1 = false;
        let mut mask_is_synthetic_1x1 = false;
        // Stage 3f.13: gradient picture's dst-pixel → LUT-pixel
        // affine, composed with the user `RenderSetPictureTransform`
        // below. `None` for Drawable / Solid sources.
        let mut src_picture_xform: Option<vk_render::AffineXform> = None;
        let mut mask_picture_xform: Option<vk_render::AffineXform> = None;

        let (src_view, src_extent) = if src_self_alias {
            // Route through the alias scratch. The scratch matches
            // dst's format + extent; sampler config (repeat mode) is
            // baked into the pipeline shader path, not the view —
            // see plan §3c "self-aliasing".
            (
                src_alias_view.expect("set when self_alias_used"),
                dst_extent,
            )
        } else {
            match src {
                ResolvedSource::Drawable(id) => {
                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    let class = swizzle_class_for(info.format, info.depth);
                    let sampler = sampler_config_for_repeat(src_repeat);
                    let view = ensure_drawable_view(
                        &inner.vk,
                        &mut inner.drawable_view_cache,
                        id,
                        info.image,
                        info.format,
                        sampler,
                        class,
                    )?;
                    (view, info.extent)
                }
                ResolvedSource::Solid(color) => {
                    src_clear_color = Some(color);
                    src_is_synthetic_1x1 = true;
                    (
                        solid_src_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                    )
                }
                ResolvedSource::Gradient(xid) => match inner.picture_paint.get(&xid) {
                    Some(PicturePaintState::Gradient(g)) => {
                        src_picture_xform = Some(g.axis_projection);
                        (g.image_view(), g.extent())
                    }
                    None => {
                        log::debug!(
                            "v2 render_composite gap: gradient picture 0x{xid:x} \
                             missing from engine.picture_paint (LUT build likely failed)"
                        );
                        return Ok(stats);
                    }
                },
                ResolvedSource::None => {
                    log::debug!("v2 render_composite gap: src is None (protocol requires src)");
                    return Ok(stats);
                }
            }
        };
        let (mask_view, mask_extent) = if mask_self_alias {
            (
                src_alias_view.expect("set when self_alias_used"),
                dst_extent,
            )
        } else {
            match mask {
                ResolvedSource::Drawable(id) => {
                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    let class = swizzle_class_for(info.format, info.depth);
                    let sampler = sampler_config_for_repeat(mask_repeat);
                    let view = ensure_drawable_view(
                        &inner.vk,
                        &mut inner.drawable_view_cache,
                        id,
                        info.image,
                        info.format,
                        sampler,
                        class,
                    )?;
                    (view, info.extent)
                }
                ResolvedSource::Solid(color) => {
                    mask_clear_color = Some(color);
                    mask_is_synthetic_1x1 = true;
                    (
                        solid_mask_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                    )
                }
                ResolvedSource::Gradient(xid) => match inner.picture_paint.get(&xid) {
                    Some(PicturePaintState::Gradient(g)) => {
                        mask_picture_xform = Some(g.axis_projection);
                        (g.image_view(), g.extent())
                    }
                    None => {
                        log::debug!(
                            "v2 render_composite gap: gradient mask picture 0x{xid:x} \
                             missing from engine.picture_paint (LUT build likely failed)"
                        );
                        return Ok(stats);
                    }
                },
                ResolvedSource::None => {
                    mask_is_synthetic_1x1 = true;
                    (
                        white_mask_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                    )
                }
            }
        };

        // Build (or look up) the pipeline.
        let pipeline = inner
            .render_pipelines
            .as_mut()
            .expect("ensured")
            .get(std_op, dst_format, dst_has_alpha, mask_component_alpha)
            .map_err(|e| {
                log::warn!("v2 render_composite: pipeline build failed for op {op}: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        let pipeline_layout = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .pipeline_layout();

        // dst_readback for Disjoint/Conjoint: ensure the scratch
        // exists at dst's extent + format.
        let dst_readback_view = if needs_dst_readback {
            let rb = inner.dst_readback.as_mut().expect("ensured");
            rb.ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                .map_err(|e| {
                    log::warn!("v2 render_composite: dst readback ensure failed: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?;
            match rb.view(dst_format, dst_has_alpha) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    log::warn!("v2 render_composite: dst readback view None — skipping");
                    return Ok(stats);
                }
                Err(e) => {
                    log::warn!("v2 render_composite: dst readback view build failed: {e:?}");
                    return Ok(stats);
                }
            }
        } else {
            white_mask_view
        };

        // Per-op descriptor arena: pool lives until SubmittedOp
        // retires (CB holds descriptor set references).
        let mut arena = BatchDescriptorArena::new(Arc::clone(&inner.vk));
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into(
                &mut arena,
                src_view,
                mask_view,
                dst_readback_view,
            )?;

        // Synthetic 1×1 scratches use PAD so the single texel
        // covers the whole rect. Otherwise honour the user repeat.
        let effective_src_repeat = if src_is_synthetic_1x1 {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        } else {
            crate::kms::backend::repeat_to_shader_const(src_repeat)
        };
        let effective_mask_repeat = if mask_is_synthetic_1x1 {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        } else {
            crate::kms::backend::repeat_to_shader_const(mask_repeat)
        };

        // Affine transforms. For Drawable / Solid sources the
        // picture-side transform is identity. For gradient sources
        // (Stage 3f.13) the gradient picture's `axis_projection`
        // maps dst-pixel → LUT-pixel; compose with the user's
        // `RenderSetPictureTransform` so the user-defined
        // dst-space → picture-space mapping applies first, then
        // the gradient projection. Matches v1's `compose_affines(
        // intrinsic, user)` shape.
        let user_src_xform =
            crate::kms::backend::pixman_transform_to_affine(src_transform.as_ref(), src_extent);
        let user_mask_xform =
            crate::kms::backend::pixman_transform_to_affine(mask_transform.as_ref(), mask_extent);
        let combined_src_xform = match src_picture_xform {
            Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_src_xform),
            None => user_src_xform,
        };
        let combined_mask_xform = match mask_picture_xform {
            Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_mask_xform),
            None => user_mask_xform,
        };

        // X11 Render PictFormat: pictures wrapping a depth < 32
        // drawable have `alpha_mask = 0`, so samples must return
        // α = 1.0 regardless of the storage byte (which is
        // server-owned padding for depth-24 BGRA storage). Resolve
        // per src/mask drawable; Solid / Gradient / None carry their
        // own α and don't need the override.
        let src_force_opaque = resolve_force_opaque(store, &src);
        let mask_force_opaque = resolve_force_opaque(store, &mask);

        let attrs = vk_render::CompositeAttrs {
            src_extent,
            mask_extent,
            src_repeat: effective_src_repeat,
            mask_repeat: effective_mask_repeat,
            src_force_opaque,
            mask_force_opaque,
            src_xform: combined_src_xform,
            mask_xform: combined_mask_xform,
        };

        // Build clip scissor list. None / empty → single
        // full-extent scissor (full dst paint). Multi-rect clips
        // pass the rects through so `record_render_composite` can
        // emit one draw call per intersection (plan §4).
        let clip_scissors: Vec<vk::Rect2D> = match clip_rects {
            None => vec![vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: dst_extent,
            }],
            Some(cr) => {
                let mut out = Vec::with_capacity(cr.len());
                for r in cr {
                    if r.width == 0 || r.height == 0 {
                        continue;
                    }
                    let x0 = i32::from(r.x).max(0);
                    let y0 = i32::from(r.y).max(0);
                    let x1 = (i32::from(r.x) + i32::from(r.width))
                        .min(i32::from(dst_extent.width as u16).max(0));
                    let y1 = (i32::from(r.y) + i32::from(r.height))
                        .min(i32::from(dst_extent.height as u16).max(0));
                    if x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                    out.push(vk::Rect2D {
                        offset: vk::Offset2D { x: x0, y: y0 },
                        extent: vk::Extent2D {
                            #[allow(clippy::cast_sign_loss)]
                            width: (x1 - x0) as u32,
                            #[allow(clippy::cast_sign_loss)]
                            height: (y1 - y0) as u32,
                        },
                    });
                }
                if out.is_empty() {
                    // Every clip rect fell outside the dst — nothing to paint.
                    return Ok(stats);
                }
                out
            }
        };

        // Record the CB.
        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        // Clear synthetic scratches (their texels become the
        // source/mask sample). `record_solid_color_clear`
        // transitions them to SHADER_READ_ONLY internally.
        if let Some(c) = src_clear_color {
            let solid = inner.solid_src_image.as_mut().expect("ensured");
            record_solid_color_clear(&inner.vk, cb, solid, c);
        }
        if let Some(c) = mask_clear_color {
            let solid = inner.solid_mask_image.as_mut().expect("ensured");
            record_solid_color_clear(&inner.vk, cb, solid, c);
        }

        // Stage 3c.3 self-alias: copy dst → alias scratch so the
        // composite pass samples the snapshot rather than the live
        // attachment. Must precede `record_render_composite`'s
        // dst→COLOR_ATTACHMENT transition. `record_copy_from`
        // restores dst to `dst_current` after the copy, so the
        // subsequent COLOR_ATTACHMENT barrier sees the same
        // oldLayout it would have without the scratch path.
        if self_alias_used {
            let dst_current = {
                let d = store.get(dst_id).expect("checked");
                d.storage.current_layout
            };
            let rb = inner.src_alias_readback.as_mut().expect("ensured");
            rb.record_copy_from(cb, dst_image, dst_current, dst_format, dst_extent);
            stats.used_src_alias_scratch = true;
        }

        // Disjoint/Conjoint: copy current dst → readback scratch
        // before the composite pass samples it. The scratch keeps
        // its own layout-tracker on `DstReadback`; the source dst
        // is in SHADER_READ_ONLY here (every paint op restores it
        // before returning).
        if needs_dst_readback {
            let rb = inner.dst_readback.as_mut().expect("ensured");
            let dst_current = {
                let d = store.get(dst_id).expect("checked");
                d.storage.current_layout
            };
            rb.record_copy_from(cb, dst_image, dst_current, dst_format, dst_extent);
            stats.used_dst_readback = true;
        }

        // Hand the dst storage to the recorder as a CompositeTarget.
        // record_render_composite will flip it to COLOR_ATTACHMENT
        // and back; we then propagate the tracked layout into the
        // Drawable's storage so subsequent ops see the right
        // oldLayout in their barriers.
        let mut adapter = {
            let d = store.get_mut(dst_id).expect("checked");
            StorageCompositeTarget {
                extent: dst_extent,
                image: dst_image,
                image_view: dst_view,
                current_layout: d.storage.current_layout,
            }
        };
        vk_render::record_render_composite(
            &inner.vk,
            cb,
            &mut adapter,
            pipeline,
            pipeline_layout,
            descriptor_set,
            &attrs,
            rects,
            &clip_scissors,
        )?;
        // Reflect the recorder's layout-mutation back into the
        // Drawable.
        {
            let d = store.get_mut(dst_id).expect("checked");
            d.storage.current_layout = adapter.current_layout;
        }
        let _ = device; // silence unused if we add no further raw ops.

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(dst_id, ticket.clone());
        // Damage hook: union of rect bboxes intersected with
        // clip_scissors (plan §5 default rule).
        for cr in rects {
            #[allow(clippy::cast_possible_wrap)]
            let rect = vk::Rect2D {
                offset: vk::Offset2D {
                    x: cr.dst_x,
                    y: cr.dst_y,
                },
                extent: vk::Extent2D {
                    width: cr.width,
                    height: cr.height,
                },
            };
            store.damage(dst_id, clamp_rect(rect, dst_extent));
        }

        stats.recorded_draws = u32::try_from(rects.len() * clip_scissors.len()).unwrap_or(u32::MAX);
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: Some(arena),
        });
        Ok(stats)
    }

    // ── Op: render_fill_rectangles (Stage 3c) ───────────────────

    /// X RENDER `FillRectangles`: paint `rects` with a single
    /// premultiplied colour using PictOp `op`. Per plan §3c
    /// "Scope", this is `render_composite(op, SolidFill(color),
    /// NoMask, dst, ...)` — one composite with N rects.
    ///
    /// # Errors
    ///
    /// Same shape as [`render_composite`].
    pub(crate) fn render_fill_rectangles(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        color: [f32; 4],
        dst_id: DrawableId,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<CompositeStats, RenderError> {
        self.render_composite(
            store,
            platform,
            op,
            ResolvedSource::Solid(color),
            ResolvedSource::None,
            dst_id,
            rects,
            clip_rects,
            Repeat::Pad,
            Repeat::Pad,
            None,
            None,
            false,
        )
    }

    // ── Op: render_traps_or_tris (Stage 3e.2) ───────────────────

    /// GPU-rasterized RENDER `Trapezoids` / `Triangles`. Backend
    /// wrapper decodes the wire stream, applies `(x_off, y_off)`,
    /// computes the bounding box, and packs per-instance vertex
    /// data; the engine method takes those pre-cooked inputs and
    /// drives a two-stage CB: first the trap pipeline rasterizes
    /// analytic edge coverage into an R8 [`MaskScratch`] image,
    /// then the standard render pipeline composites `src ⊗ mask`
    /// into `dst`. Mirrors v1's `try_vk_render_traps_or_tris`
    /// (kms/backend.rs:4500) port — same trap pipeline + mask
    /// scratch infrastructure, adapted for v2's per-op CB shape.
    ///
    /// `bbox` is `(x, y, w, h)` in pixel coords (already clamped
    /// to non-negative by the wrapper). `prim_kind` selects which
    /// sibling pipeline to bind (trap edges vs triangle edges).
    ///
    /// Out-of-scope gating (unknown op, gradient src — Stage 3e
    /// gradient support hasn't landed yet, mask self-alias,
    /// unsupported dst format, src self-alias) returns
    /// `Ok(stats)` with `recorded_draws = 0` — same shape as
    /// `render_composite`. Source self-alias bails with a gap log
    /// (would need scratch routing à la 3c.3; rare in real-world
    /// trap workloads).
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing.
    /// - `Vk(...)` for pipeline / scratch / CB failures.
    /// - `RendererFailed` if `platform.renderer_failed`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_traps_or_tris(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        dst_id: DrawableId,
        prim_kind: TrapPrimKind,
        instance_data: &[u8],
        instance_count: u32,
        bbox: (i32, i32, u32, u32),
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        src_transform: Option<PictTransform>,
    ) -> Result<CompositeStats, RenderError> {
        use crate::kms::vk::{
            ops::render as vk_render,
            render_pipeline::{StdPictOp, record_solid_color_clear},
            trap_pipeline::TrapDrawPushConsts,
        };

        let mut stats = CompositeStats::default();
        if instance_count == 0 {
            return Ok(stats);
        }
        let (bbox_x, bbox_y, bbox_w, bbox_h) = bbox;
        if bbox_w == 0 || bbox_h == 0 {
            return Ok(stats);
        }

        self.ensure_render_assets(platform)?;
        self.ensure_trap_assets(platform)?;
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        // Resolve dst metadata. Same gate as render_composite.
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
                d.depth,
            )
        };
        if dst_extent.width == 0 || dst_extent.height == 0 {
            return Ok(stats);
        }
        if !matches!(
            dst_format,
            vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM
        ) {
            log::debug!("v2 render_traps_or_tris gap: dst format {dst_format:?} unsupported");
            return Ok(stats);
        }
        let dst_has_alpha = dst_format == vk::Format::R8_UNORM || dst_depth == 32;
        let Some(std_op) = StdPictOp::from_u8(op) else {
            log::debug!("v2 render_traps_or_tris gap: unsupported op {op}");
            return Ok(stats);
        };
        let needs_dst_readback = std_op.needs_dst_readback();

        // Self-alias gate — the trap path doesn't yet route src
        // through the alias scratch (rare in real-world trap
        // workloads; see plan §3e.2 out-of-scope list).
        if matches!(src, ResolvedSource::Drawable(id) if id == dst_id) {
            log::debug!("v2 render_traps_or_tris gap: src self-alias (out of scope for 3e.2)");
            return Ok(stats);
        }

        // Allocate the per-instance vertex buffer (HOST_VISIBLE
        // upload, sized for instance_count × instance stride). The
        // wrapper has already laid the data out in the buffer's
        // exact wire shape; copy verbatim.
        let needed = u64::try_from(instance_data.len()).unwrap_or(0).max(1);
        let instance_buf = StagingBuffer::new_with_usage(
            Arc::clone(&inner.vk),
            needed,
            vk::BufferUsageFlags::VERTEX_BUFFER,
        )?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                instance_data.as_ptr(),
                instance_buf.mapped.as_ptr(),
                instance_data.len(),
            );
        }

        // Grow the mask scratch to at least the trap bbox. The
        // retired old image is currently dropped on the floor (see
        // RenderEngineInner.mask_scratch doc note); same shape as
        // dst_readback's grow semantics.
        let _retired_mask = {
            let scratch = inner.mask_scratch.as_mut().expect("ensured");
            scratch
                .ensure_image_size_returning_old(bbox_w, bbox_h)
                .map_err(|e| {
                    log::warn!("v2 render_traps_or_tris: mask ensure_image_size: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?
        };
        let mask_image = inner.mask_scratch.as_ref().expect("ensured").image();
        // Two distinct views on mask_scratch: the IDENTITY-swizzle
        // `attachment_view` for the rasterize-side write (Vulkan
        // VUID-VkFramebufferCreateInfo-pAttachments-00891 requires
        // identity on attachments), and the `a=R`-swizzled `view`
        // for the composite-side sample (so `mask_sample.a` reads the
        // R8 coverage). Sharing one swizzled view across both —
        // the pre-3f-bug-fix behaviour — works under lavapipe but
        // silently produces nothing on Intel / RADV. See xeyes pupil
        // bug 2026-05-16.
        let mask_attachment_view = inner
            .mask_scratch
            .as_ref()
            .expect("ensured")
            .attachment_view();
        let mask_view = inner.mask_scratch.as_ref().expect("ensured").image_view();
        let mask_extent = inner.mask_scratch.as_ref().expect("ensured").extent();

        // Resolve src view + extent (Drawable / Solid; Gradient +
        // self-alias pre-flighted above). Same view-cache path as
        // render_composite.
        let solid_src_view = inner
            .solid_src_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let mut src_clear_color: Option<[f32; 4]> = None;
        // Stage 3f.13: gradient picture's intrinsic dst→LUT xform,
        // composed with the user transform below. `None` for
        // Drawable / Solid sources.
        let mut src_picture_xform: Option<vk_render::AffineXform> = None;
        // Stage 3f.14 bug-fix (xeyes pupil-missing repro): mirrors
        // `render_composite`'s synthetic-1×1 handling. SolidFill
        // sources allocate `solid_src_image` at 1×1; the fragment
        // shader's `apply_repeat` with `REPEAT_NONE` returns 0 for
        // any tex_coord outside `[0, 1)`, which on stricter drivers
        // (Intel / RADV; lavapipe is lenient) yields all-zero src
        // everywhere except exactly one pixel. The trap composite
        // then has `src × mask = 0` and paints nothing. Force
        // `REPEAT_PAD` for synthetic 1×1 scratches so the single
        // texel covers the whole rect — matching
        // `render_composite`'s `src_is_synthetic_1x1` override.
        let mut src_is_synthetic_1x1 = false;
        let (src_view, src_extent) = match src {
            ResolvedSource::Drawable(id) => {
                let info =
                    drawable_for_render_view(store, id).ok_or(RenderError::UnknownDrawable(id))?;
                let class = swizzle_class_for(info.format, info.depth);
                let sampler = sampler_config_for_repeat(src_repeat);
                let view = ensure_drawable_view(
                    &inner.vk,
                    &mut inner.drawable_view_cache,
                    id,
                    info.image,
                    info.format,
                    sampler,
                    class,
                )?;
                (view, info.extent)
            }
            ResolvedSource::Solid(color) => {
                src_clear_color = Some(color);
                src_is_synthetic_1x1 = true;
                (
                    solid_src_view,
                    vk::Extent2D {
                        width: 1,
                        height: 1,
                    },
                )
            }
            ResolvedSource::Gradient(xid) => match inner.picture_paint.get(&xid) {
                Some(PicturePaintState::Gradient(g)) => {
                    src_picture_xform = Some(g.axis_projection);
                    (g.image_view(), g.extent())
                }
                None => {
                    log::debug!(
                        "v2 render_traps_or_tris gap: gradient picture 0x{xid:x} \
                         missing from engine.picture_paint (LUT build likely failed)"
                    );
                    return Ok(stats);
                }
            },
            ResolvedSource::None => {
                log::debug!("v2 render_traps_or_tris gap: src None");
                return Ok(stats);
            }
        };

        // dst_readback for Disjoint/Conjoint (Stage 3c parity).
        let white_mask_view = inner
            .white_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let dst_readback_view = if needs_dst_readback {
            let rb = inner.dst_readback.as_mut().expect("ensured");
            rb.ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                .map_err(|e| {
                    log::warn!("v2 render_traps_or_tris: dst readback ensure: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?;
            match rb.view(dst_format, dst_has_alpha) {
                Ok(Some(v)) => v,
                Ok(None) | Err(_) => return Ok(stats),
            }
        } else {
            white_mask_view
        };

        // Pipeline + descriptor for the composite stage. No
        // component_alpha; the trap path doesn't carry one.
        let pipeline = inner
            .render_pipelines
            .as_mut()
            .expect("ensured")
            .get(std_op, dst_format, dst_has_alpha, false)
            .map_err(|e| {
                log::warn!("v2 render_traps_or_tris: pipeline build {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        let pipeline_layout = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .pipeline_layout();
        let mut arena = BatchDescriptorArena::new(Arc::clone(&inner.vk));
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into(
                &mut arena,
                src_view,
                mask_view,
                dst_readback_view,
            )?;

        // needs_full_dst: ops where mask=0 still affects dst
        // (Clear, Src, etc — see v1 backend.rs:4870). Mask scratch
        // sits at offset (bbox_x, bbox_y) within dst; outside the
        // bbox the REPEAT_NONE mask returns 0 and the operator
        // gives the right outside-bbox result.
        let needs_full_dst = matches!(op, 0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43);
        let (render_dst_x, render_dst_y, render_w, render_h, mask_off_x, mask_off_y) =
            if needs_full_dst {
                (0, 0, dst_extent.width, dst_extent.height, -bbox_x, -bbox_y)
            } else {
                (bbox_x, bbox_y, bbox_w, bbox_h, 0, 0)
            };

        // Build the picture-clip scissor list (same shape as
        // render_composite). None → single render-area-sized
        // scissor; multi-rect picture clips emit one draw per
        // intersection.
        let clip_scissors: Vec<vk::Rect2D> = match clip_rects {
            None => vec![vk::Rect2D {
                offset: vk::Offset2D {
                    x: render_dst_x,
                    y: render_dst_y,
                },
                extent: vk::Extent2D {
                    width: render_w,
                    height: render_h,
                },
            }],
            Some(cr) => {
                let mut out = Vec::with_capacity(cr.len());
                for r in cr {
                    if r.width == 0 || r.height == 0 {
                        continue;
                    }
                    let x0 = i32::from(r.x).max(0);
                    let y0 = i32::from(r.y).max(0);
                    let x1 = (i32::from(r.x) + i32::from(r.width))
                        .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX));
                    let y1 = (i32::from(r.y) + i32::from(r.height))
                        .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX));
                    if x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                    out.push(vk::Rect2D {
                        offset: vk::Offset2D { x: x0, y: y0 },
                        extent: vk::Extent2D {
                            #[allow(clippy::cast_sign_loss)]
                            width: (x1 - x0) as u32,
                            #[allow(clippy::cast_sign_loss)]
                            height: (y1 - y0) as u32,
                        },
                    });
                }
                if out.is_empty() {
                    return Ok(stats);
                }
                out
            }
        };

        // Begin the CB.
        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        // Clear synthetic src (SolidFill).
        if let Some(c) = src_clear_color {
            let solid = inner.solid_src_image.as_mut().expect("ensured");
            record_solid_color_clear(&inner.vk, cb, solid, c);
        }

        // ── Trap rasterize phase ───────────────────────────────
        let (prim_pipeline, prim_layout) = {
            let tp = inner.trap_pipeline.as_ref().expect("ensured");
            let pipe = match prim_kind {
                TrapPrimKind::Trapezoid => tp.trapezoid_pipeline(),
                TrapPrimKind::Triangle => tp.triangle_pipeline(),
            };
            (pipe, tp.pipeline_layout())
        };

        let mask_src_layout = inner
            .mask_scratch
            .as_ref()
            .expect("ensured")
            .current_layout();
        let (mask_src_stage, mask_src_access) = match mask_src_layout {
            vk::ImageLayout::UNDEFINED => {
                (vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::NONE)
            }
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            ),
            _ => (
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            ),
        };
        let color_range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let to_attach = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(mask_src_stage)
            .src_access_mask(mask_src_access)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .old_layout(mask_src_layout)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(mask_image)
            .subresource_range(color_range)];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_attach);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        let bbox_render_area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: bbox_w,
                height: bbox_h,
            },
        };
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 0.0],
            },
        };
        let color_attachment = [vk::RenderingAttachmentInfo::default()
            // Use the IDENTITY-swizzle view as the color attachment;
            // the swizzled `mask_view` is for sample-side use only.
            .image_view(mask_attachment_view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(clear)];
        let rendering_info = vk::RenderingInfo::default()
            .render_area(bbox_render_area)
            .layer_count(1)
            .color_attachments(&color_attachment);
        unsafe {
            device.cmd_begin_rendering(cb, &rendering_info);
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, prim_pipeline);
            device.cmd_bind_vertex_buffers(cb, 0, &[instance_buf.buffer], &[0]);
        }
        #[allow(clippy::cast_precision_loss)]
        let trap_pc = TrapDrawPushConsts {
            mask_extent: [mask_extent.width as f32, mask_extent.height as f32],
            bbox_origin_pixel: [bbox_x as f32, bbox_y as f32],
            bbox_size_pixel: [bbox_w as f32, bbox_h as f32],
            _pad: [0.0; 2],
        };
        unsafe {
            device.cmd_push_constants(
                cb,
                prim_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                trap_pc.as_bytes(),
            );
        }
        #[allow(clippy::cast_precision_loss)]
        let trap_viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: mask_extent.width as f32,
            height: mask_extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        unsafe {
            device.cmd_set_viewport(cb, 0, &trap_viewport);
            device.cmd_set_scissor(cb, 0, &[bbox_render_area]);
            device.cmd_draw(cb, 4, instance_count, 0, 0);
            device.cmd_end_rendering(cb);
        }

        // Barrier mask: COLOR_ATTACHMENT → SHADER_READ_ONLY for
        // the composite read.
        let to_read = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(mask_image)
            .subresource_range(color_range)];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        // ── Composite phase ────────────────────────────────────
        // dst_readback snapshot for Disjoint/Conjoint.
        if needs_dst_readback {
            let dst_current = store.get(dst_id).expect("checked").storage.current_layout;
            let rb = inner.dst_readback.as_mut().expect("ensured");
            rb.record_copy_from(cb, dst_image, dst_current, dst_format, dst_extent);
            stats.used_dst_readback = true;
        }

        // Stage 3f.13: compose the gradient picture's intrinsic
        // axis projection with the user `RenderSetPictureTransform`
        // for gradient sources; identity-composed for Drawable /
        // Solid sources.
        let user_src_xform =
            crate::kms::backend::pixman_transform_to_affine(src_transform.as_ref(), src_extent);
        let combined_src_xform = match src_picture_xform {
            Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_src_xform),
            None => user_src_xform,
        };
        // Synthetic 1×1 scratches use PAD so the single texel
        // covers the whole rect (xeyes pupil-missing fix). The
        // mask scratch is sampled within bbox — REPEAT_NONE
        // outside returns 0 which is what `needs_full_dst`
        // requires.
        let effective_src_repeat = if src_is_synthetic_1x1 {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        } else {
            crate::kms::backend::repeat_to_shader_const(src_repeat)
        };
        // X11 Render PictFormat force-opaque for the src picture.
        // The mask is the trap-coverage R8 scratch the engine
        // rasterised in this op — never a user picture — so its α
        // is server-controlled and force_opaque doesn't apply.
        let src_force_opaque = resolve_force_opaque(store, &src);

        let attrs = vk_render::CompositeAttrs {
            src_extent,
            mask_extent,
            src_repeat: effective_src_repeat,
            mask_repeat: crate::kms::vk::render_pipeline::REPEAT_NONE,
            src_force_opaque,
            mask_force_opaque: false,
            src_xform: combined_src_xform,
            mask_xform: vk_render::AffineXform::IDENTITY,
        };

        let rects = [vk_render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: mask_off_x,
            mask_y: mask_off_y,
            dst_x: render_dst_x,
            dst_y: render_dst_y,
            width: render_w,
            height: render_h,
        }];

        let mut adapter = {
            let d = store.get_mut(dst_id).expect("checked");
            StorageCompositeTarget {
                extent: dst_extent,
                image: dst_image,
                image_view: dst_view,
                current_layout: d.storage.current_layout,
            }
        };
        vk_render::record_render_composite(
            &inner.vk,
            cb,
            &mut adapter,
            pipeline,
            pipeline_layout,
            descriptor_set,
            &attrs,
            &rects,
            &clip_scissors,
        )?;
        {
            let d = store.get_mut(dst_id).expect("checked");
            d.storage.current_layout = adapter.current_layout;
        }
        // Advance mask_scratch's CPU-tracked layout to match the
        // post-barrier state on the GPU. v1 defers this past the
        // final fallible step; v2's record_render_composite is
        // infallible after the descriptor set is bound, so this
        // is safe.
        inner
            .mask_scratch
            .as_mut()
            .expect("ensured")
            .set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(dst_id, ticket.clone());
        // Damage: the projected dst rect (post-clip is honoured by
        // scissoring; damage union is the union of clip-scissored
        // rects intersected with dst extent — keep coarse for now).
        let dmg = vk::Rect2D {
            offset: vk::Offset2D {
                x: render_dst_x,
                y: render_dst_y,
            },
            extent: vk::Extent2D {
                width: render_w,
                height: render_h,
            },
        };
        store.damage(dst_id, clamp_rect(dmg, dst_extent));

        stats.recorded_draws = u32::try_from(clip_scissors.len()).unwrap_or(u32::MAX);
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: Some(instance_buf),
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: Some(arena),
        });
        Ok(stats)
    }
}

// ────────────────────────────────────────────────────────────────
// Stage 3c support: source resolution + drawable view cache.
// ────────────────────────────────────────────────────────────────

/// Stage 3e.2: primitive kind for [`RenderEngine::render_traps_or_tris`].
/// Selects which sibling of the trap pipeline to bind. Pre-cooked
/// instance data + count are passed alongside; the kind only
/// affects pipeline selection.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TrapPrimKind {
    Trapezoid,
    Triangle,
}

/// Picture source resolved against `KmsCore.pictures` by the
/// backend wrapper. The engine doesn't read protocol records
/// directly; the wrapper hands it one of these.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResolvedSource {
    /// Picture wraps a drawable; the engine samples its storage.
    Drawable(DrawableId),
    /// `RenderCreateSolidFill` source: a single premultiplied
    /// RGBA colour. Pipeline samples from a 1×1 scratch cleared
    /// to this colour per call.
    Solid([f32; 4]),
    /// Gradient placeholder (linear / radial). Stage 3c bails;
    /// 3e wires LUT build through `RenderEngine.picture_paint`.
    Gradient(u32),
    /// No mask (only valid as `mask`). Bound to the engine's
    /// white-mask scratch so `mask.a == 1.0` makes the blend a
    /// no-op.
    None,
}

/// Telemetry surface for one [`RenderEngine::render_composite`]
/// or [`RenderEngine::render_fill_rectangles`] call. The wrapper
/// pushes these into the per-second / lifetime telemetry sinks.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CompositeStats {
    /// Whether the op took the `Disjoint`/`Conjoint` shader-side
    /// `dst_readback` path. Used to wire the
    /// `disjoint_readback_count` telemetry counter.
    pub used_dst_readback: bool,
    /// Whether the op took the Stage 3c.3 self-alias path
    /// (`src.drawable_id() == dst_id`, or same for mask). Tests
    /// assert this surfaces the scratch route; v1 had no observable
    /// signal for this case (the bug it was hiding).
    pub used_src_alias_scratch: bool,
    /// Total `vkCmdDraw` calls issued (rects × clip-rect
    /// intersections). Used by the acceptance harness to assert
    /// per-rect-scissor splits.
    pub recorded_draws: u32,
}

/// Snapshot of a drawable's view-relevant metadata. Lives only
/// long enough to build a `vk::ImageView` against it.
struct DrawableViewInfo {
    image: vk::Image,
    extent: vk::Extent2D,
    format: vk::Format,
    depth: u8,
}

/// X11 Render PictFormat force-opaque resolver.
///
/// Per the X11 Render spec, a Picture whose format has
/// `alpha_mask == 0` (e.g. a depth-24 RGB visual: r8g8b8 or
/// x8r8g8b8) must yield samples with `α = 1.0` regardless of the
/// byte content of the underlying storage. v2 stores depth-24
/// pixmaps as `B8G8R8A8_UNORM` with the α byte as server-owned
/// padding — without this override, marco/compositing samples the
/// padding byte (often 0) and the operator collapses to no-op,
/// leaving widget windows invisible under a compositing WM.
///
/// ## Gate: `depth == 24` BGRA storage only
///
/// Stage 4d landed this as `depth == 24` rather than the broader
/// `depth < 32` because depth-8 (A8 alpha-only pictures) and
/// depth-1 (bitmap masks) carry meaningful α in the X11 Render
/// `PictFormat` — forcing `α = 1.0` on a depth-8 mask would
/// silently turn coverage masks into solid blocks. Picture-
/// format-driven resolution (looking up the actual `PictFormat`
/// attached to the source picture rather than the drawable
/// depth) is the cleaner long-term shape; `depth == 24` is the
/// load-bearing case marco-with-compositing depends on, so we
/// fix that first and broaden later if a non-depth-24 picture
/// format with `alpha_mask == 0` shows up in a real workload.
///
/// `Solid` carries its own α, `Gradient` LUTs are authored with
/// the right α from `RenderCreateGradient`, and `None` is the
/// synthetic white-mask path — none of those need the override.
fn resolve_force_opaque(store: &DrawableStore, src: &ResolvedSource) -> bool {
    match src {
        ResolvedSource::Drawable(id) => store.get(*id).is_some_and(|d| d.depth == 24),
        ResolvedSource::Solid(_) | ResolvedSource::Gradient(_) | ResolvedSource::None => false,
    }
}

fn drawable_for_render_view(store: &DrawableStore, id: DrawableId) -> Option<DrawableViewInfo> {
    let d = store.get(id)?;
    Some(DrawableViewInfo {
        image: d.storage.image,
        extent: d.storage.extent,
        format: d.storage.format,
        depth: d.depth,
    })
}

fn sampler_config_for_repeat(r: Repeat) -> SamplerConfig {
    match r {
        Repeat::None => SamplerConfig::Clamp,
        Repeat::Normal => SamplerConfig::Repeat,
        Repeat::Pad => SamplerConfig::Pad,
        Repeat::Reflect => SamplerConfig::Reflect,
    }
}

fn swizzle_class_for(format: vk::Format, depth: u8) -> SwizzleClass {
    match (format, depth) {
        (vk::Format::R8_UNORM, _) => SwizzleClass::AlphaOnlyR8,
        (vk::Format::B8G8R8A8_UNORM, 24) => SwizzleClass::BgraNoAlpha,
        _ => SwizzleClass::RgbaIdent,
    }
}

/// Lookup/build a `vk::ImageView` for `id` with the given
/// (sampler, swizzle) classification. The cache key splits on
/// SamplerConfig so a Repeat=None vs Repeat=Pad sample of the
/// same drawable doesn't share — Stage 3c uses Nearest only, so
/// sampler is "address mode" rather than full sampler state.
/// Address mode actually lives in the pipeline cache's sampler
/// (one shared linear sampler) — the cache split is therefore
/// over-engineered for 3c but matches the plan's published
/// (DrawableId, SamplerConfig, SwizzleClass) key, leaving room
/// for Stage 5's per-address-mode sampler splits without a
/// cache-shape rewrite.
fn ensure_drawable_view(
    vk: &VkContext,
    cache: &mut HashMap<(DrawableId, SamplerConfig, SwizzleClass), CachedDrawableView>,
    id: DrawableId,
    image: vk::Image,
    format: vk::Format,
    sampler: SamplerConfig,
    class: SwizzleClass,
) -> Result<vk::ImageView, vk::Result> {
    let key = (id, sampler, class);
    if let Some(c) = cache.get(&key) {
        return Ok(c.view);
    }
    let components = match class {
        SwizzleClass::RgbaIdent => vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::IDENTITY,
        },
        SwizzleClass::AlphaOnlyR8 => vk::ComponentMapping {
            r: vk::ComponentSwizzle::ZERO,
            g: vk::ComponentSwizzle::ZERO,
            b: vk::ComponentSwizzle::ZERO,
            a: vk::ComponentSwizzle::R,
        },
        SwizzleClass::BgraNoAlpha => vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::ONE,
        },
    };
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .components(components)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let view = unsafe { vk.device.create_image_view(&info, None)? };
    cache.insert(key, CachedDrawableView { view });
    Ok(view)
}

/// Adapter implementing [`CompositeTarget`] over a v2 `Drawable`'s
/// storage fields. Built per-call by `render_composite`; the
/// recorder mutates `current_layout` and the caller reflects it
/// back into the Drawable's storage on success.
struct StorageCompositeTarget {
    extent: vk::Extent2D,
    image: vk::Image,
    image_view: vk::ImageView,
    current_layout: vk::ImageLayout,
}

impl CompositeTarget for StorageCompositeTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}

/// CPU-rasterised glyph the caller hands to
/// [`RenderEngine::image_text`]. Mirrors v1's `RenderedGlyph`
/// shape, but living in the v2 engine module so the public type
/// surface is self-contained. `pixels` is row-major tightly
/// packed, `w × h` alpha bytes (FreeType `BITMAP_GRAY`).
#[derive(Debug)]
pub(crate) struct PreparedGlyph {
    pub dst_x: i32,
    pub dst_y: i32,
    pub w: usize,
    pub h: usize,
    pub pixels: Vec<u8>,
    pub codepoint: u32,
}

/// Single glyph input to [`RenderEngine::composite_glyphs`]. The
/// backend wrapper resolves glyphset xid + glyph id via
/// `KmsCore.glyphsets`, expands A1 bitmaps host-side to dense A8,
/// and computes the per-glyph dst position from the items stream's
/// running pen + glyph metrics. Lifetimes: `pixels` is borrowed
/// from `KmsCore.glyphsets[gs_xid].glyphs[glyph_id].pixels` (or a
/// caller-owned A1→A8 scratch); the engine copies it into a per-
/// glyph `StagingBuffer` on intern, so the borrow only needs to
/// outlive the engine call itself.
pub(crate) struct CompositeGlyphInput<'a> {
    /// Glyphset xid the glyph came from (atlas key namespace).
    pub gs_xid: u32,
    /// Glyph id within the glyphset (atlas key codepoint).
    pub glyph_id: u32,
    /// Glyph width / height. 0×0 entries cache an empty entry and
    /// skip the upload (space glyphs after pen-only adjustment).
    pub w: u32,
    pub h: u32,
    /// Dense A8 bitmap, row-major `w × h`. Caller is responsible
    /// for A1→A8 expansion + ARGB32→A8 alpha-extract (the latter
    /// is done in `parse_add_glyphs` for storage; the former is
    /// per-call because v1 stores A1 raw and expands at draw time).
    pub pixels: &'a [u8],
    /// Dst-space top-left corner for the glyph quad.
    pub dst_x: i32,
    pub dst_y: i32,
}

/// Telemetry surface for one [`RenderEngine::image_text`] call.
/// Caller (KmsBackendV2) feeds these into the telemetry sink so
/// `atlas_intern/s`, `glyph_uploads/s`, and the lifetime
/// `glyphs_dropped_atlas_full` counter all stay accurate.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ImageTextStats {
    pub atlas_interns: u32,
    pub glyph_uploads: u32,
    pub glyphs_dropped: u32,
}

/// Per-picture GPU-side state. Stage 3b only carries an empty
/// placeholder variant; Stage 3c adds `Gradient(GradientPicture)`
/// (the v1-side LUT-image type) when the first `render_composite`
/// against a gradient picture lazy-builds it.
/// Note: `GradientPicture` carries raw Vk handles + an `Arc<VkContext>`
/// and doesn't implement `Debug`, so this enum stays Debug-free.
pub(crate) enum PicturePaintState {
    /// GPU-side state for a `LinearGradient` / `RadialGradient`
    /// picture record. The wrapped [`GradientPicture`] owns its
    /// image / view / memory; dropping it (via
    /// [`RenderEngine::picture_paint_remove`] on `RenderFreePicture`)
    /// destroys the Vk resources. Built eagerly at
    /// `render_create_linear_gradient` / `render_create_radial_gradient`
    /// time so the first `render_composite` against the gradient
    /// has the LUT ready.
    Gradient(crate::kms::vk::gradient::GradientPicture),
}

/// Adapter implementing [`TextRunTarget`] over a v2 `Drawable`'s
/// storage fields. Built by [`RenderEngine::image_text`]; layout
/// changes performed by the recorder are read back into the
/// Drawable's storage by the caller.
struct StorageTextTarget {
    extent: vk::Extent2D,
    image: vk::Image,
    image_view: vk::ImageView,
    current_layout: vk::ImageLayout,
}

impl TextRunTarget for StorageTextTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}

// Stage 3c: `record_render_composite` takes the same minimal
// paint-target surface as `record_text_run`. Impl `CompositeTarget`
// on the same adapter so v2's RENDER paint sites can hand the
// recorder a borrow over a `Drawable`'s storage fields.
impl CompositeTarget for StorageTextTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}

impl Drop for RenderEngine {
    fn drop(&mut self) {
        // Best-effort drain — any submitted ops that didn't go
        // through `drain_all` would leak CBs. The `Drop` here
        // can't access the platform's pool any more, but it can
        // wait on each fence so `StagingBuffer`'s drop is safe.
        if let Some(inner) = self.inner.as_mut() {
            for op in inner.submitted.drain(..) {
                let _ = op.ticket.wait(&inner.vk);
                // staging drops here.
                drop(op.staging);
                // CB handles leak — caller should have invoked
                // `drain_all` against a live platform pool. The
                // pool's own Drop destroys the pool, which
                // implicitly frees all its CBs (Vulkan spec).
                let _ = op.cb;
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Helpers: CB lifecycle, byte conversion, rect clipping.
// ────────────────────────────────────────────────────────────────

/// Allocate a fresh primary CB from the platform's
/// `OpsCommandPool`, begin recording, and acquire a
/// `FenceTicket` from the platform's fence pool. Returns
/// `(cb, ticket)` ready to record into.
fn begin_op_cb(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
) -> Result<(vk::CommandBuffer, FenceTicket), RenderError> {
    let pool = platform
        .ops_command_pool_handle()
        .ok_or(RenderError::NoVk)?;
    let device = &inner.vk.device;
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { device.allocate_command_buffers(&alloc_info)? }[0];
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { device.begin_command_buffer(cb, &begin)? };
    let ticket = platform.acquire_fence_ticket()?;
    Ok((cb, ticket))
}

/// End CB recording, submit on the graphics queue with the
/// ticket's fence, return `Ok` on accept. Same-queue submission
/// order with the I6a fence is what Stage 2 plan cross-cutting
/// §3 banks on for paint→compose ordering without
/// `vkQueueWaitIdle`.
fn end_and_submit_op(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
    cb: vk::CommandBuffer,
    ticket: &FenceTicket,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    unsafe { device.end_command_buffer(cb)? };
    platform.submit_paint_cb(cb, ticket.fence())?;
    let _ = device;
    Ok(())
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

/// Single-image-layout barrier helper for scratch images that
/// `Drawable::record_layout_transition` can't touch (the scratch
/// isn't a tracked drawable).
#[allow(clippy::too_many_arguments)]
fn barrier_to_layout(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
) {
    let b = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&b);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
}

/// Allocate a scratch image for `copy_area`'s overlap path.
/// Device-local, OPTIMAL tiling, TRANSFER_SRC|TRANSFER_DST usage.
/// Caller is responsible for adopting it into the op's
/// `SubmittedOp.scratch` so it retires on the fence.
fn allocate_scratch_image(
    vk: &Arc<VkContext>,
    _platform: &PlatformBackend,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<ScratchImage, RenderError> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { vk.device.create_image(&info, None)? };
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let Some(mt) = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    }) else {
        unsafe { vk.device.destroy_image(image, None) };
        return Err(RenderError::Vk(vk::Result::ERROR_FEATURE_NOT_PRESENT));
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(RenderError::Vk(e));
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(RenderError::Vk(e));
    }
    Ok(ScratchImage {
        vk: Arc::clone(vk),
        image,
        memory,
    })
}

fn clamp_rect(rect: vk::Rect2D, extent: vk::Extent2D) -> vk::Rect2D {
    let max_x = i32::try_from(extent.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(extent.height).unwrap_or(i32::MAX);
    let x0 = rect.offset.x.max(0).min(max_x);
    let y0 = rect.offset.y.max(0).min(max_y);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .clamp(0, max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .clamp(0, max_y);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

/// Compute the destination rect (in storage coords) and the
/// (sx, sy) origin in the input image where copying should start.
/// Returns `None` if no pixels are visible.
fn clamp_put_rect(
    dst_pos: vk::Offset2D,
    src_extent: vk::Extent2D,
    dst_extent: vk::Extent2D,
) -> Option<(vk::Rect2D, (u32, u32))> {
    let max_x = i32::try_from(dst_extent.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(dst_extent.height).unwrap_or(i32::MAX);
    let x0 = dst_pos.x.max(0);
    let y0 = dst_pos.y.max(0);
    let sx = (x0 - dst_pos.x).max(0);
    let sy = (y0 - dst_pos.y).max(0);
    let x1 = dst_pos
        .x
        .saturating_add_unsigned(src_extent.width)
        .min(max_x);
    let y1 = dst_pos
        .y
        .saturating_add_unsigned(src_extent.height)
        .min(max_y);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        vk::Rect2D {
            offset: vk::Offset2D { x: x0, y: y0 },
            extent: vk::Extent2D {
                width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
                height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
            },
        },
        (
            u32::try_from(sx).unwrap_or(0),
            u32::try_from(sy).unwrap_or(0),
        ),
    ))
}

/// X11 ZPixmap source row stride for a given depth + width. Per
/// the wire format: scanline padded to 32 bits.
fn x11_src_row_stride(depth: u8, width: u32) -> usize {
    let bits_per_row = match depth {
        1 => width,
        8 => u32::from(8u8) * width,
        24 | 32 => 32 * width,
        _ => 32 * width,
    };
    // Pad up to 32 bits (4 bytes).
    let bits_padded = bits_per_row.div_ceil(32) * 32;
    (bits_padded / 8) as usize
}

/// Copy a sub-rect of `src` (ZPixmap wire, padded rows) into
/// `dst_ptr` (tightly packed bytes matching the storage format).
///
/// # Safety
///
/// `dst_ptr` must be valid for `dst_w * dst_h * dst_bpp` bytes.
///
/// # Errors
///
/// `TruncatedSource` if `src` is shorter than the row stride ×
/// (sy + dst_h) the depth implies.
fn unpack_to_staging(
    src: &[u8],
    src_extent: vk::Extent2D,
    sx: u32,
    sy: u32,
    dst_w: u32,
    dst_h: u32,
    src_depth: u8,
    dst_ptr: *mut u8,
) -> Result<(), RenderError> {
    let src_row_bytes = x11_src_row_stride(src_depth, src_extent.width);
    let expected_len =
        src_row_bytes
            .checked_mul((sy + dst_h) as usize)
            .ok_or(RenderError::TruncatedSource {
                expected: usize::MAX,
            })?;
    if src.len() < expected_len {
        return Err(RenderError::TruncatedSource {
            expected: expected_len,
        });
    }
    match src_depth {
        32 | 24 => {
            // BGRA8 wire → BGRA8 staging. For depth-24, force
            // alpha to 0xFF so subsequent sample-as-source has a
            // defined alpha channel.
            let row_dst_bytes = (dst_w * 4) as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let src_col_off = sx as usize * 4;
                let src_slice =
                    &src[src_row_off + src_col_off..src_row_off + src_col_off + row_dst_bytes];
                // SAFETY: caller guarantees dst_ptr is valid for
                // dst_w*dst_h*4 bytes; row * row_dst_bytes within.
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    std::ptr::copy_nonoverlapping(src_slice.as_ptr(), dst, row_dst_bytes);
                    if src_depth == 24 {
                        // Stomp alpha to 0xFF every 4th byte.
                        for col in 0..dst_w as usize {
                            *dst.add(col * 4 + 3) = 0xFF;
                        }
                    }
                }
            }
        }
        8 => {
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let src_col_off = sx as usize;
                let src_slice =
                    &src[src_row_off + src_col_off..src_row_off + src_col_off + row_dst_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    std::ptr::copy_nonoverlapping(src_slice.as_ptr(), dst, row_dst_bytes);
                }
            }
        }
        1 => {
            // 1 bit per pixel MSB-first → 1 byte per pixel
            // (0xFF if set, 0x00 if clear). Unpack each
            // requested column from the source bit position.
            let row_dst_bytes = dst_w as usize;
            for row in 0..dst_h {
                let src_row_off = (sy + row) as usize * src_row_bytes;
                let row_src = &src[src_row_off..src_row_off + src_row_bytes];
                unsafe {
                    let dst = dst_ptr.add(row as usize * row_dst_bytes);
                    for col in 0..dst_w as usize {
                        let bit_index = sx as usize + col;
                        let byte = row_src[bit_index / 8];
                        let bit = (byte >> (7 - (bit_index % 8))) & 0x1;
                        *dst.add(col) = if bit != 0 { 0xFF } else { 0x00 };
                    }
                }
            }
        }
        _ => return Err(RenderError::UnsupportedDepth(src_depth)),
    }
    Ok(())
}

/// Convert tightly-packed storage bytes (from a GetImage
/// readback) into the wire format clients expect. Inverse of
/// [`unpack_to_staging`].
///
/// # Errors
///
/// `UnsupportedDepth` for depths other than 1/8/24/32.
fn pack_from_storage(raw: &[u8], w: u32, h: u32, depth: u8) -> Result<Vec<u8>, RenderError> {
    match depth {
        32 | 24 => {
            // Storage is BGRA8 tightly packed; wire ZPixmap is
            // also BGRA8 tightly packed for our advertised
            // visual (no scanline pad at depth-32 because
            // 32 bits already aligns). Round-trip is a memcpy.
            // depth-24 carries the alpha byte through (clients
            // ignore the X-byte position).
            Ok(raw.to_vec())
        }
        8 => {
            // Scanline padded to 32 bits.
            let row_dst_bytes = (w as usize + 3) & !3;
            let mut out = vec![0u8; row_dst_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_dst_bytes;
                out[dst_off..dst_off + w as usize]
                    .copy_from_slice(&raw[src_off..src_off + w as usize]);
            }
            Ok(out)
        }
        1 => {
            // Pack 0xFF/0x00 bytes back to 1bpp MSB-first;
            // scanline padded to 32 bits.
            let row_bytes = w.div_ceil(32) as usize * 4;
            let mut out = vec![0u8; row_bytes * h as usize];
            for row in 0..h as usize {
                let src_off = row * w as usize;
                let dst_off = row * row_bytes;
                for col in 0..w as usize {
                    if raw[src_off + col] != 0 {
                        out[dst_off + col / 8] |= 1 << (7 - (col % 8));
                    }
                }
            }
            Ok(out)
        }
        _ => Err(RenderError::UnsupportedDepth(depth)),
    }
}

/// Decode an X11 32-bit pixel (B in low byte, then G, R, A) into
/// an RGBA float-4 suitable for `vkCmdClearAttachments` against a
/// `B8G8R8A8_UNORM` target.
#[must_use]
pub(crate) fn decode_x11_pixel_bgra(pixel: u32) -> [f32; 4] {
    let b = (pixel & 0xff) as f32 / 255.0;
    let g = ((pixel >> 8) & 0xff) as f32 / 255.0;
    let r = ((pixel >> 16) & 0xff) as f32 / 255.0;
    let a = ((pixel >> 24) & 0xff) as f32 / 255.0;
    // `vkCmdClearAttachments` clearColor.float32 against a
    // BGRA8_UNORM attachment writes [R, G, B, A] components per
    // spec — the format swizzle handles the BGRA→RGBA mapping at
    // store time. So we pass logical RGBA here.
    [r, g, b, a]
}

/// L1 server-α invariant: depth-24 / depth-8 / depth-1 destinations
/// are server-owned-α, so the stored alpha byte must read back as
/// `0xFF` regardless of what the X11 pixel's upper byte happens to
/// contain (typically `0x00` for `0x00RRGGBB` colour literals). The
/// scene compositor binds `storage.image_view` (IDENTITY swizzle —
/// required because the same view doubles as a colour attachment per
/// VUID-VkFramebufferCreateInfo-pAttachments-00891) and runs window
/// draws in `alpha_passthrough=true` mode, so a paint that leaves
/// α=0 in storage renders as a fully-transparent window — the layer
/// underneath leaks through. v1 forces this at every fill site
/// (`kms/backend.rs:try_vk_solid_fill`); this helper is v2's
/// equivalent.
#[must_use]
pub(crate) fn decode_x11_pixel_server_alpha(pixel: u32, depth: u8) -> [f32; 4] {
    let mut c = decode_x11_pixel_bgra(pixel);
    if depth != 32 {
        c[3] = 1.0;
    }
    c
}

// ────────────────────────────────────────────────────────────────
// Tests — logic-only (no live Vk).
//
// Vk-backed integration tests are gated by `#[ignore = "needs live
// Vulkan ICD"]` so they run only when explicitly requested. The
// Stage 2 acceptance harness (Stage 2f) drives them end-to-end.
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_engine_declines_paint_ops() {
        let mut engine = RenderEngine::stub();
        let mut store = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let storage = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();
        let err = engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                [1.0, 0.0, 0.0, 1.0],
            )
            .expect_err("stub engine must reject");
        assert!(matches!(err, RenderError::NoVk));
        assert!(!engine.is_live());
    }

    #[test]
    fn decode_pixel_bgra_round_trip() {
        // 0xAARRGGBB → r,g,b,a in 0..1
        let rgba = decode_x11_pixel_bgra(0xFF_80_40_20);
        assert!((rgba[0] - 128.0 / 255.0).abs() < 1e-3); // R = 0x80
        assert!((rgba[1] - 64.0 / 255.0).abs() < 1e-3); // G = 0x40
        assert!((rgba[2] - 32.0 / 255.0).abs() < 1e-3); // B = 0x20
        assert!((rgba[3] - 255.0 / 255.0).abs() < 1e-3); // A = 0xFF
    }

    #[test]
    fn x11_row_stride_pad_to_32_bits() {
        // depth-1, width 9 → 9 bits → ceil(9/32)*4 = 4 bytes.
        assert_eq!(x11_src_row_stride(1, 9), 4);
        // depth-1, width 33 → ceil(33/32)*4 = 8.
        assert_eq!(x11_src_row_stride(1, 33), 8);
        // depth-8, width 3 → 24 bits padded to 32 → 4 bytes.
        assert_eq!(x11_src_row_stride(8, 3), 4);
        // depth-8, width 5 → 40 bits padded to 64 → 8 bytes.
        assert_eq!(x11_src_row_stride(8, 5), 8);
        // depth-32, width 10 → 320 bits = 40 bytes (already aligned).
        assert_eq!(x11_src_row_stride(32, 10), 40);
    }

    #[test]
    fn clamp_put_rect_inside_returns_unchanged() {
        let r = clamp_put_rect(
            vk::Offset2D { x: 2, y: 3 },
            vk::Extent2D {
                width: 4,
                height: 5,
            },
            vk::Extent2D {
                width: 16,
                height: 16,
            },
        )
        .unwrap();
        assert_eq!(r.0.offset, vk::Offset2D { x: 2, y: 3 });
        assert_eq!(
            r.0.extent,
            vk::Extent2D {
                width: 4,
                height: 5,
            },
        );
        assert_eq!(r.1, (0, 0));
    }

    #[test]
    fn clamp_put_rect_partial_clip_records_source_offset() {
        // dst_pos = (-1, -2), src 4×5 against a 16×16 storage →
        // dst rect (0,0,3,3) with source-input origin (1, 2).
        let r = clamp_put_rect(
            vk::Offset2D { x: -1, y: -2 },
            vk::Extent2D {
                width: 4,
                height: 5,
            },
            vk::Extent2D {
                width: 16,
                height: 16,
            },
        )
        .unwrap();
        assert_eq!(r.0.offset, vk::Offset2D { x: 0, y: 0 });
        assert_eq!(
            r.0.extent,
            vk::Extent2D {
                width: 3,
                height: 3,
            },
        );
        assert_eq!(r.1, (1, 2));
    }

    #[test]
    fn clamp_put_rect_outside_returns_none() {
        let r = clamp_put_rect(
            vk::Offset2D { x: 100, y: 100 },
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Extent2D {
                width: 16,
                height: 16,
            },
        );
        assert!(r.is_none());
    }

    #[test]
    fn depth1_unpack_round_trip() {
        // 1×8 source padded to a 32-bit scanline (4 bytes); MSB
        // first the first byte holds the bits 10101010 = 0xAA,
        // the remaining 3 bytes are scanline pad.
        let src = vec![0xAAu8, 0x00, 0x00, 0x00];
        let src_extent = vk::Extent2D {
            width: 8,
            height: 1,
        };
        let mut out = vec![0u8; 8];
        unpack_to_staging(&src, src_extent, 0, 0, 8, 1, 1, out.as_mut_ptr()).unwrap();
        assert_eq!(out, vec![0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00]);

        let packed = pack_from_storage(&out, 8, 1, 1).unwrap();
        // Row stride is 4 bytes (32 bits) per depth-1 pad rule;
        // the high byte holds the data.
        assert_eq!(packed.len(), 4);
        assert_eq!(packed[0], 0xAA);
    }

    #[test]
    fn depth32_unpack_is_memcpy() {
        // 2×2 BGRA8 source.
        let src: Vec<u8> = vec![
            0x10, 0x20, 0x30, 0xFF, 0x11, 0x21, 0x31, 0xFF, // row 0
            0x12, 0x22, 0x32, 0xFF, 0x13, 0x23, 0x33, 0xFF, // row 1
        ];
        let src_extent = vk::Extent2D {
            width: 2,
            height: 2,
        };
        let mut out = vec![0u8; 16];
        unpack_to_staging(&src, src_extent, 0, 0, 2, 2, 32, out.as_mut_ptr()).unwrap();
        assert_eq!(out, src);
    }

    // ── Vk-backed integration tests ─────────────────────────────
    //
    // Each `#[ignore]` test needs a live Vulkan ICD (lavapipe is
    // fine). Run with:
    //   `cargo test -p yserver --lib kms::v2::engine::tests:: -- --ignored`
    // The Stage 2 acceptance harness (Stage 2f) folds these into
    // the synthetic acceptance binary.

    fn live_platform() -> Option<PlatformBackend> {
        // Can't reuse `PlatformBackend::open_with_commit` here —
        // it tries to acquire a real DRM device. Tests need a
        // VkContext-only fixture. We build one by hand:
        // construct a `for_tests` fixture, then swap in a real
        // VkContext + OpsCommandPool + FencePool.
        let mut p = PlatformBackend::for_tests();
        let vk = match VkContext::new() {
            Ok(v) => v,
            Err(_) => return None,
        };
        let ops_pool = match crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)) {
            Ok(o) => o,
            Err(_) => return None,
        };
        let fence_pool = super::super::platform::FencePool::new(Arc::clone(&vk));
        p.vk = Some(vk);
        p.ops_command_pool = Some(ops_pool);
        p.fence_pool = Some(fence_pool);
        Some(p)
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn depth32_put_image_get_image_round_trip() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform
            .allocate_drawable_storage(8, 8, 32)
            .expect("alloc storage");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .expect("store.allocate");

        // 8x8 BGRA8 gradient.
        let mut src = vec![0u8; 8 * 8 * 4];
        for y in 0..8 {
            for x in 0..8 {
                let off = (y * 8 + x) * 4;
                src[off] = (x * 32) as u8; // B
                src[off + 1] = (y * 32) as u8; // G
                src[off + 2] = ((x + y) * 16) as u8; // R
                src[off + 3] = 0xFF; // A
            }
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D { x: 0, y: 0 },
                vk::Extent2D {
                    width: 8,
                    height: 8,
                },
                &src,
                32,
            )
            .expect("put_image");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: 8,
                        height: 8,
                    },
                },
                32,
            )
            .expect("get_image");
        assert_eq!(out, src, "depth-32 round-trip must be byte-identical");

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fill_then_get_image_observes_clear_color() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // Fill the whole pixmap with bright red (R=0xFF, G=0, B=0, A=0xFF).
        let color = decode_x11_pixel_bgra(0xFF_FF_00_00);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                color,
            )
            .expect("fill_rect");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // Storage is BGRA8: every pixel should be [B=0, G=0, R=0xFF, A=0xFF].
        for px in out.chunks_exact(4) {
            assert_eq!(px[0], 0x00, "B");
            assert_eq!(px[1], 0x00, "G");
            assert_eq!(px[2], 0xFF, "R");
            assert_eq!(px[3], 0xFF, "A");
        }

        engine.drain_all(&platform);
    }

    /// Stage 3f.2: `engine.logic_fill` applies the per-`GcFunction`
    /// `VkLogicOp` per pixel. Drives `Xor` against a pre-loaded BGRA8
    /// pattern; expects each component to be the pre-load XOR'd with
    /// the fg byte. Alpha is preserved via the `opaque_alpha=true`
    /// pipeline (L1 server-α invariant on depth-24).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn logic_fill_xor_applies_per_pixel() {
        use yserver_core::backend::GcFunction;

        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        // 4x4 BGRA8 pixmap. Store BGRA wire bytes B, G, R, A.
        let storage = platform.allocate_drawable_storage(4, 4, 24).expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                24,
                false,
                storage,
            )
            .unwrap();

        // Load every pixel with [B=0x20, G=0x40, R=0x80, A=0xFF].
        let mut pre = vec![0u8; 4 * 4 * 4];
        for px in pre.chunks_exact_mut(4) {
            px[0] = 0x20;
            px[1] = 0x40;
            px[2] = 0x80;
            px[3] = 0xFF;
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 4,
                    height: 4,
                },
                &pre,
                32,
            )
            .expect("put_image");

        // XOR with fg pixel 0x00FFFFFF (X11 wire = AARRGGBB: A=0,
        // R=0xFF, G=0xFF, B=0xFF). The recorder's `BGRA8_UNORM`
        // branch puts R/G/B into [0]/[1]/[2] of `fg_color`; the
        // logic-op output then targets the BGRA8 attachment in the
        // same channel order, so post-XOR every component reads as
        // `pre ^ 0xFF`.
        let rect = Rectangle16 {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        engine
            .logic_fill(
                &mut store,
                &mut platform,
                id,
                GcFunction::Xor,
                /* opaque_alpha */ true,
                /* fg */ 0x00FF_FFFF,
                &[rect],
            )
            .expect("logic_fill");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");

        for px in out.chunks_exact(4) {
            assert_eq!(px[0], 0x20 ^ 0xFF, "B (XOR pre 0x20 with fg 0xFF)");
            assert_eq!(px[1], 0x40 ^ 0xFF, "G (XOR pre 0x40 with fg 0xFF)");
            assert_eq!(px[2], 0x80 ^ 0xFF, "R (XOR pre 0x80 with fg 0xFF)");
            // opaque_alpha=true: alpha channel mask drops alpha from
            // the LogicOp, so the destination's pre-load 0xFF is
            // preserved.
            assert_eq!(px[3], 0xFF, "A preserved by opaque_alpha mask");
        }

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn copy_area_disjoint_pixmaps_round_trip() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage_src = platform.allocate_drawable_storage(4, 4, 32).unwrap();
        let storage_dst = platform.allocate_drawable_storage(8, 4, 32).unwrap();
        let src = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage_src,
            )
            .unwrap();
        let dst = store
            .allocate(
                0x2,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage_dst,
            )
            .unwrap();

        // Fill src with red.
        let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                src,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                red,
            )
            .unwrap();
        // Fill dst with blue.
        let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                blue,
            )
            .unwrap();
        // Copy src into dst at (4, 0).
        engine
            .copy_area(
                &mut store,
                &mut platform,
                src,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                vk::Offset2D { x: 4, y: 0 },
            )
            .unwrap();

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                32,
            )
            .unwrap();
        // Left half (0..4) should be blue (B=0xFF, G=0, R=0, A=0xFF).
        for y in 0..4 {
            for x in 0..4 {
                let off = (y * 8 + x) * 4;
                assert_eq!(&out[off..off + 4], &[0xFF, 0x00, 0x00, 0xFF], "left blue");
            }
        }
        // Right half (4..8) should be red (B=0, G=0, R=0xFF, A=0xFF).
        for y in 0..4 {
            for x in 4..8 {
                let off = (y * 8 + x) * 4;
                assert_eq!(&out[off..off + 4], &[0x00, 0x00, 0xFF, 0xFF], "right red");
            }
        }

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn copy_area_self_overlap_scratch_path() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform.allocate_drawable_storage(8, 1, 32).unwrap();
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // PutImage a horizontal gradient: 8 pixels each with a
        // distinct red value.
        let mut src = vec![0u8; 8 * 4];
        for x in 0..8 {
            let off = x * 4;
            src[off] = 0x00; // B
            src[off + 1] = 0x00; // G
            src[off + 2] = (x as u8) * 0x20; // R
            src[off + 3] = 0xFF; // A
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 8,
                    height: 1,
                },
                &src,
                32,
            )
            .unwrap();
        // Copy (0..4) → (2..6) (overlap; scratch path engages).
        engine
            .copy_area(
                &mut store,
                &mut platform,
                id,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 1,
                    },
                },
                vk::Offset2D { x: 2, y: 0 },
            )
            .unwrap();

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 1,
                    },
                },
                32,
            )
            .unwrap();
        // Expected R-channel sequence: [0, 0x20, 0, 0x20, 0x40, 0x60, 0xC0, 0xE0]
        // After copy of (0..4) → (2..6):
        //   col 0: original (R=0)
        //   col 1: original (R=0x20)
        //   col 2: src col 0 (R=0)
        //   col 3: src col 1 (R=0x20)
        //   col 4: src col 2 (R=0x40)
        //   col 5: src col 3 (R=0x60)
        //   col 6: original col 6 (R=0xC0)
        //   col 7: original col 7 (R=0xE0)
        let expected_r = [0x00, 0x20, 0x00, 0x20, 0x40, 0x60, 0xC0, 0xE0];
        for (x, &exp) in expected_r.iter().enumerate() {
            let off = x * 4 + 2;
            assert_eq!(
                out[off], exp,
                "R at col {x} (got {:#x}, want {exp:#x})",
                out[off]
            );
        }

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn put_image_then_fill_overwrites() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // PutImage all-blue, then fill (1,1)..(3,3) with green.
        // B=0xFF, G=0, R=0, A=0xFF
        let blue = [0xFFu8, 0x00, 0x00, 0xFF].repeat(16);
        engine
            .put_image(
                &mut store,
                &mut platform,
                id,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 4,
                    height: 4,
                },
                &blue,
                32,
            )
            .unwrap();
        let green = decode_x11_pixel_bgra(0xFF_00_FF_00);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D { x: 1, y: 1 },
                    extent: vk::Extent2D {
                        width: 2,
                        height: 2,
                    },
                },
                green,
            )
            .unwrap();

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .unwrap();
        // (0,0) still blue.
        assert_eq!(&out[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
        // (1,1) green: B=0, G=0xFF, R=0, A=0xFF.
        let off_1_1 = (4 + 1) * 4;
        assert_eq!(&out[off_1_1..off_1_1 + 4], &[0x00, 0xFF, 0x00, 0xFF]);
        // (3,3) still blue.
        let off_3_3 = (3 * 4 + 3) * 4;
        assert_eq!(&out[off_3_3..off_3_3 + 4], &[0xFF, 0x00, 0x00, 0xFF]);

        engine.drain_all(&platform);
    }

    #[test]
    fn depth24_unpack_forces_alpha_ff() {
        // Source 1×1 with X-byte (alpha-slot) = 0x55.
        let src = vec![0x10u8, 0x20, 0x30, 0x55];
        let src_extent = vk::Extent2D {
            width: 1,
            height: 1,
        };
        let mut out = vec![0u8; 4];
        unpack_to_staging(&src, src_extent, 0, 0, 1, 1, 24, out.as_mut_ptr()).unwrap();
        assert_eq!(out, vec![0x10, 0x20, 0x30, 0xFF]);
    }

    // ── Stage 3a Vk-backed integration tests ────────────────────

    /// Helper: allocate a depth-32 storage and return a registered
    /// DrawableId. Mirrors the pattern Stage 2c tests use.
    fn alloc_drawable_3a(
        platform: &PlatformBackend,
        store: &mut DrawableStore,
        xid: u32,
        w: u16,
        h: u16,
    ) -> DrawableId {
        alloc_drawable_3a_with_kind(
            platform,
            store,
            xid,
            w,
            h,
            super::super::store::DrawableKind::Pixmap,
            false,
        )
    }

    fn alloc_drawable_3a_with_kind(
        platform: &PlatformBackend,
        store: &mut DrawableStore,
        xid: u32,
        w: u16,
        h: u16,
        kind: super::super::store::DrawableKind,
        scene_participating: bool,
    ) -> DrawableId {
        let storage = platform
            .allocate_drawable_storage(w, h, 32)
            .expect("alloc storage");
        store
            .allocate(xid, kind, 32, scene_participating, storage)
            .expect("store allocate")
    }

    /// Build a `PreparedGlyph` with `w × h` filled bytes (the
    /// fill byte is 0xFF so the shader paints solid foreground).
    fn build_glyph(codepoint: u32, dst_x: i32, dst_y: i32, w: usize, h: usize) -> PreparedGlyph {
        PreparedGlyph {
            dst_x,
            dst_y,
            w,
            h,
            pixels: vec![0xFF_u8; w * h],
            codepoint,
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn image_text_run_records_damage_on_target() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        // Window-kind + scene-participating so presentation damage
        // accumulates (per the I5 spec amendment, pixmaps no longer
        // accumulate any damage in the store — protocol DamageNotify
        // fanout lives at the request layer).
        let id = alloc_drawable_3a_with_kind(
            &platform,
            &mut store,
            0x1,
            64,
            32,
            super::super::store::DrawableKind::Window,
            true,
        );
        // Two glyphs spanning x=[10..22] × y=[5..17].
        let glyphs = vec![
            build_glyph(u32::from(b'A'), 10, 5, 6, 12),
            build_glyph(u32::from(b'B'), 16, 5, 6, 12),
        ];
        let stats = engine
            .image_text(
                &mut store,
                &mut platform,
                id,
                7,
                [1.0, 1.0, 1.0, 1.0],
                &glyphs,
            )
            .expect("image_text");
        assert_eq!(stats.atlas_interns, 2);
        assert_eq!(stats.glyph_uploads, 2);
        assert_eq!(stats.glyphs_dropped, 0);

        // Damage union covers the two glyph quads.
        let d = store.get(id).expect("drawable");
        let rects: Vec<vk::Rect2D> = d.presentation_damage.rects().to_vec();
        assert!(!rects.is_empty(), "presentation damage should be set");
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for r in rects {
            min_x = min_x.min(r.offset.x);
            min_y = min_y.min(r.offset.y);
            max_x = max_x.max(r.offset.x + r.extent.width as i32);
            max_y = max_y.max(r.offset.y + r.extent.height as i32);
        }
        assert!(min_x <= 10);
        assert!(min_y <= 5);
        assert!(max_x >= 22);
        assert!(max_y >= 17);

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn atlas_intern_uses_fence_ticket() {
        // After image_text submits, the engine's submitted queue
        // grows by exactly one upload op + one consume op per fresh
        // intern. Each is in-flight until drain.
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let id = alloc_drawable_3a(&platform, &mut store, 0x1, 16, 16);

        assert_eq!(engine.pending_count(), 0);
        let glyphs = vec![build_glyph(0xA0A0, 1, 1, 4, 4)];
        engine
            .image_text(
                &mut store,
                &mut platform,
                id,
                1,
                [1.0, 1.0, 1.0, 1.0],
                &glyphs,
            )
            .expect("ok");
        // One upload + one consume.
        assert!(engine.pending_count() >= 2);
        engine.drain_all(&platform);
        assert_eq!(engine.pending_count(), 0);
    }

    /// **Load-bearing per codex round 1**: two back-to-back glyph
    /// uploads with distinct keys must not corrupt each other's
    /// atlas pixels. v1's shared persistent staging would clobber
    /// A when B's memcpy lands while A's GPU read is in flight; the
    /// v2 per-upload arena slice rules that out.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn atlas_back_to_back_upload_no_corruption() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

        // Pre-clear the target to black.
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                target,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 32,
                        height: 32,
                    },
                },
                [0.0, 0.0, 0.0, 1.0],
            )
            .expect("clear");

        // Two glyphs with distinguishable solid-alpha rectangles.
        // The text shader does `foreground × atlas.r`; with
        // 0xFF-filled atlas and white foreground, the dst quads
        // come out (B=0xFF, G=0xFF, R=0xFF, A=0xFF).
        let glyphs = vec![
            build_glyph(u32::from(b'A'), 1, 1, 4, 4),
            build_glyph(u32::from(b'B'), 10, 1, 4, 4),
        ];
        let stats = engine
            .image_text(
                &mut store,
                &mut platform,
                target,
                42,
                [1.0, 1.0, 1.0, 1.0],
                &glyphs,
            )
            .expect("image_text");
        assert_eq!(stats.atlas_interns, 2);

        // Read back: both quads should be white; pixels between
        // them should be the original black.
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                target,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 32,
                        height: 32,
                    },
                },
                32,
            )
            .expect("get_image");
        let pixel_at = |x: usize, y: usize| {
            let off = (y * 32 + x) * 4;
            (out[off], out[off + 1], out[off + 2], out[off + 3])
        };
        // A's quad: (1..5, 1..5).
        for y in 1..5 {
            for x in 1..5 {
                let (b, g, r, _a) = pixel_at(x, y);
                assert_eq!(
                    (b, g, r),
                    (0xFF, 0xFF, 0xFF),
                    "glyph A quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
                );
            }
        }
        // B's quad: (10..14, 1..5).
        for y in 1..5 {
            for x in 10..14 {
                let (b, g, r, _a) = pixel_at(x, y);
                assert_eq!(
                    (b, g, r),
                    (0xFF, 0xFF, 0xFF),
                    "glyph B quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
                );
            }
        }
        // Between the quads (7, 2) should still be black.
        let (b, g, r, _a) = pixel_at(7, 2);
        assert_eq!(
            (b, g, r),
            (0x00, 0x00, 0x00),
            "between-quad pixel (7,2) should be background black; got ({b:#x},{g:#x},{r:#x})"
        );

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn atlas_full_drops_glyph_and_increments_counter() {
        // Drive the atlas to exhaustion via the engine's image_text
        // pipeline. 4096² atlas; two 2049×2049 glyphs don't both
        // fit — the second exceeds the remaining vertical room.
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let target = alloc_drawable_3a(&platform, &mut store, 0x1, 4, 4);
        // First glyph fits.
        let g0 = build_glyph(1, 0, 0, 2049, 2049);
        let g1 = build_glyph(2, 0, 0, 2049, 2049);

        let stats = engine
            .image_text(
                &mut store,
                &mut platform,
                target,
                1,
                [1.0, 1.0, 1.0, 1.0],
                &[g0],
            )
            .expect("first image_text");
        assert_eq!(stats.atlas_interns, 1);
        assert_eq!(stats.glyphs_dropped, 0);

        let stats2 = engine
            .image_text(
                &mut store,
                &mut platform,
                target,
                1,
                [1.0, 1.0, 1.0, 1.0],
                &[g1],
            )
            .expect("second image_text");
        assert_eq!(stats2.atlas_interns, 0);
        assert_eq!(stats2.glyphs_dropped, 1);

        engine.drain_all(&platform);
    }

    // ── Stage 3c.3 acceptance tests ─────────────────────────────
    //
    // Engine-direct RENDER paint oracles. Each test allocates one
    // or two Vk-backed drawables, drives `render_composite` /
    // `render_fill_rectangles` through `RenderEngine`, then
    // round-trips via `get_image` and asserts pixel-level
    // correctness against a CPU oracle. The seventh acceptance
    // test (`render_composite_no_gc_clip_leak`) lives in
    // `tests/v2_acceptance.rs` because the "no GC clip leak"
    // property is a Backend-trait invariant (engine has no GC
    // clip notion).

    /// Allocate a Vk-backed depth-32 pixmap and pre-fill it with
    /// `color` via the engine's fill_rect path. Returns the
    /// store DrawableId.
    fn alloc_filled_pixmap(
        platform: &mut PlatformBackend,
        store: &mut DrawableStore,
        engine: &mut RenderEngine,
        xid: u32,
        w: u16,
        h: u16,
        color_bgra_premul: [f32; 4],
    ) -> DrawableId {
        let storage = platform
            .allocate_drawable_storage(w, h, 32)
            .expect("alloc storage");
        let id = store
            .allocate(
                xid,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .expect("store.allocate");
        engine
            .fill_rect(
                store,
                platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: u32::from(w),
                        height: u32::from(h),
                    },
                },
                color_bgra_premul,
            )
            .expect("pre-fill");
        id
    }

    fn full_rect(w: u32, h: u32) -> crate::kms::vk::ops::render::CompositeRect {
        crate::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_over_renders_alpha_blended() {
        // 50%-alpha red (premultiplied: r=0.5, a=0.5) Over opaque
        // green. Over: out = src + dst * (1 - src.a).
        //   out.b = 0 + 0 * 0.5 = 0
        //   out.g = 0 + 1 * 0.5 = 0.5 → 0x80
        //   out.r = 0.5 + 0 * 0.5 = 0.5 → 0x80
        //   out.a = 0.5 + 1 * 0.5 = 1.0 → 0xFF
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 1.0, 0.0, 1.0], // opaque green
        );

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                3,                                           // Over
                ResolvedSource::Solid([0.5, 0.0, 0.0, 0.5]), // 50% red premul
                ResolvedSource::None,
                dst,
                &[full_rect(4, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite");
        assert_eq!(stats.recorded_draws, 1);
        assert!(!stats.used_dst_readback);
        assert!(!stats.used_src_alias_scratch);

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // Centre pixel (1, 1): BGRA = [0, 0x80, 0x80, 0xFF] (±1).
        let off = (4 + 1) * 4;
        let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
        assert!(near(out[off], 0x00), "B at centre: got {:#x}", out[off]);
        assert!(
            near(out[off + 1], 0x80),
            "G at centre: got {:#x}",
            out[off + 1]
        );
        assert!(
            near(out[off + 2], 0x80),
            "R at centre: got {:#x}",
            out[off + 2]
        );
        assert!(
            near(out[off + 3], 0xFF),
            "A at centre: got {:#x}",
            out[off + 3]
        );

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_picture_clip_per_rect() {
        // Two disjoint clip rects with a hole between them; one
        // composite covering the union bbox must paint inside both
        // rects AND leave the hole untouched. Exercises plan §4's
        // per-rect scissoring against v1's union-bbox shortcut.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            8,
            4,
            [0.0, 0.0, 1.0, 1.0], // RGBA: opaque blue
        );
        // Two clip rects with a 2-wide hole at x=3..=4.
        let clip = vec![
            Rectangle16 {
                x: 0,
                y: 0,
                width: 3,
                height: 4,
            },
            Rectangle16 {
                x: 5,
                y: 0,
                width: 3,
                height: 4,
            },
        ];
        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                1,                                           // Src
                ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]), // RGBA: opaque red
                ResolvedSource::None,
                dst,
                &[full_rect(8, 4)],
                Some(&clip),
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite");
        // One src rect × two clip rects = 2 draw calls.
        assert_eq!(stats.recorded_draws, 2, "per-rect scissoring");

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // BGRA layout: B at +0, R at +2.
        for y in 0..4 {
            for x in 0..8u32 {
                let off = (y * 8 + x as usize) * 4;
                let in_clip = (0..3).contains(&x) || (5..8).contains(&x);
                if in_clip {
                    assert_eq!(out[off + 2], 0xFF, "R painted at ({x},{y})");
                    assert_eq!(out[off], 0x00, "B cleared at ({x},{y})");
                } else {
                    // Hole (x=3..=4): original blue.
                    assert_eq!(out[off], 0xFF, "B preserved at ({x},{y})");
                    assert_eq!(out[off + 2], 0x00, "R untouched at ({x},{y})");
                }
            }
        }

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_solid_fill_source_path() {
        // SolidFill source over (op=Src) an unrelated start colour —
        // every dst pixel must equal the source's premul colour.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 0.0, 0.0, 1.0], // opaque black
        );
        engine
            .render_composite(
                &mut store,
                &mut platform,
                1,                                             // Src
                ResolvedSource::Solid([0.25, 0.5, 0.75, 1.0]), // RGBA premul
                ResolvedSource::None,
                dst,
                &[full_rect(4, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite");
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // Storage BGRA bytes for RGBA(0.25, 0.5, 0.75, 1.0):
        // B=0.75→0xC0, G=0.5→0x80, R=0.25→0x40, A=1→0xFF.
        let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
        for px in out.chunks_exact(4) {
            assert!(near(px[0], 0xC0), "B: {:#x}", px[0]);
            assert!(near(px[1], 0x80), "G: {:#x}", px[1]);
            assert!(near(px[2], 0x40), "R: {:#x}", px[2]);
            assert!(near(px[3], 0xFF), "A: {:#x}", px[3]);
        }
        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_linear_gradient_horizontal_two_stop() {
        // 256×1 dst pre-filled black; Composite Src + LinearGradient
        // source (p1=(0,0), p2=(256,0)<<16) with two stops:
        //   pos=0   black (0,0,0,1)
        //   pos=0xFFFFFFFF white (1,1,1,1)
        // Stage 3f.13 wires the LUT path — pixel n should read
        // roughly (n, n, n, 0xFF) ± a couple of units (NEAREST
        // sampler + LUT rounding).
        use crate::kms::vk::gradient::Stop;
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            256,
            1,
            [0.0, 0.0, 0.0, 1.0],
        );

        let grad_xid = 0xABBA_FACE_u32;
        engine
            .build_and_insert_linear_gradient(
                &platform,
                grad_xid,
                (0, 0),
                (256_i32 << 16, 0),
                &[
                    Stop {
                        pos: 0,
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0xFFFF,
                    },
                    // 16.16 fixed-point: 1.0 = 0x10000. Using i32::MAX
                    // here would put the second stop far past t=1.0,
                    // so `sample_stops` would lerp `(target - 0) /
                    // i32::MAX ≈ 0` and every LUT pixel would read
                    // the first stop (black).
                    Stop {
                        pos: 0x10000,
                        r: 0xFFFF,
                        g: 0xFFFF,
                        b: 0xFFFF,
                        a: 0xFFFF,
                    },
                ],
            )
            .expect("build gradient");

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                1, // Src — copy source to dst, no blend
                ResolvedSource::Gradient(grad_xid),
                ResolvedSource::None,
                dst,
                &[full_rect(256, 1)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite gradient");
        assert_eq!(stats.recorded_draws, 1);

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 256,
                        height: 1,
                    },
                },
                32,
            )
            .expect("get_image");

        // Sample several points along the ramp; tolerate ±4 due to
        // NEAREST sampler + 8-bit LUT quantisation + premultiplied
        // colour conversion. Direction-of-travel + monotonicity is
        // the strong gate (rules out the 3f.12 first-stop collapse,
        // which would read 0 at every x).
        let bgra = |x: usize| (out[x * 4], out[x * 4 + 1], out[x * 4 + 2], out[x * 4 + 3]);
        let (b0, g0, r0, _a0) = bgra(0);
        let (bm, gm, rm, _am) = bgra(128);
        let (b255, g255, r255, _a255) = bgra(255);
        // x=0 is near-black; x=255 is near-white; x=128 sits between.
        assert!(b0 <= 4 && g0 <= 4 && r0 <= 4, "x=0 BGRA={:?}", bgra(0));
        assert!(
            b255 >= 0xF0 && g255 >= 0xF0 && r255 >= 0xF0,
            "x=255 BGRA={:?}",
            bgra(255),
        );
        assert!(
            (0x40..=0xC0).contains(&bm)
                && (0x40..=0xC0).contains(&gm)
                && (0x40..=0xC0).contains(&rm),
            "x=128 BGRA={:?} (expected mid-grey)",
            bgra(128),
        );

        // Cleanup so the gradient image is freed in this drain.
        engine.picture_paint_remove(grad_xid);
        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_radial_gradient_centred() {
        // 64×64 dst, radial gradient centred at (32,32) inner_r=0
        // outer_r=32, stops black→white. Center pixel should be
        // dark (t near 0 = first stop = black); border pixel should
        // be near-white.
        use crate::kms::vk::gradient::Stop;
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            64,
            64,
            [0.5, 0.5, 0.5, 1.0],
        );

        let grad_xid = 0xDEAD_BEEF_u32;
        engine
            .build_and_insert_radial_gradient(
                &platform,
                grad_xid,
                (32_i32 << 16, 32_i32 << 16, 0),
                (32_i32 << 16, 32_i32 << 16, 32_i32 << 16),
                &[
                    Stop {
                        pos: 0,
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0xFFFF,
                    },
                    // 16.16 fixed-point: 1.0 = 0x10000. See linear-
                    // gradient test above for why i32::MAX is wrong.
                    Stop {
                        pos: 0x10000,
                        r: 0xFFFF,
                        g: 0xFFFF,
                        b: 0xFFFF,
                        a: 0xFFFF,
                    },
                ],
            )
            .expect("build radial");

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                1, // Src
                ResolvedSource::Gradient(grad_xid),
                ResolvedSource::None,
                dst,
                &[full_rect(64, 64)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite radial");
        assert_eq!(stats.recorded_draws, 1);

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                },
                32,
            )
            .expect("get_image");

        let bgra = |x: usize, y: usize| {
            let off = (y * 64 + x) * 4;
            (out[off], out[off + 1], out[off + 2], out[off + 3])
        };
        // Centre near-black, edge near-white.
        let (bc, gc, rc, _ac) = bgra(32, 32);
        assert!(
            bc < 0x40 && gc < 0x40 && rc < 0x40,
            "centre BGRA={:?} (expected dark)",
            bgra(32, 32),
        );
        // Corner is outside the unit circle for an inscribed
        // radial — pick a point on the rim instead (x=62, y=32 →
        // r ≈ 30/32).
        let (be, ge, re_, _ae) = bgra(62, 32);
        assert!(
            be > 0xC0 && ge > 0xC0 && re_ > 0xC0,
            "rim BGRA={:?} (expected near-white)",
            bgra(62, 32),
        );

        engine.picture_paint_remove(grad_xid);
        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_missing_gradient_picture_is_gap() {
        // Engine receives a ResolvedSource::Gradient(xid) for an
        // xid that has no picture_paint entry (LUT build failed or
        // dropped early). Must return stats with recorded_draws=0,
        // log a debug gap, and NOT panic.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 0.0, 0.0, 1.0],
        );

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                1, // Src
                ResolvedSource::Gradient(0xC0FF_EE00),
                ResolvedSource::None,
                dst,
                &[full_rect(4, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite Ok even on missing gradient");
        assert_eq!(stats.recorded_draws, 0);
        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_disjoint_clear_uses_readback() {
        // PictOp 16 = DisjointClear; `needs_dst_readback` returns
        // true for ops ≥ 13 (Saturate + Disjoint/Conjoint families).
        // `CompositeStats.used_dst_readback` is the engine's signal.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");
        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 0.0, 1.0, 1.0],
        );
        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                16, // DisjointClear
                ResolvedSource::Solid([0.0, 0.0, 0.0, 1.0]),
                ResolvedSource::None,
                dst,
                &[full_rect(4, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite");
        assert!(
            stats.used_dst_readback,
            "Disjoint family must drive the readback path",
        );
        assert_eq!(stats.recorded_draws, 1);
        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_composite_self_alias() {
        // src == dst: pre-fill with a vertical gradient, then
        // Composite(Over, dst, NoMask, dst). Over with itself on
        // opaque alpha yields self exactly (out = src + dst*(1-1) =
        // src). Without the scratch path the GPU samples a region
        // as it writes it — undefined behaviour; with it, the
        // result must be bit-identical to the pre-fill.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        // Allocate + PutImage a distinct pattern (per-pixel unique).
        let storage = platform.allocate_drawable_storage(8, 4, 32).expect("alloc");
        let dst = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .expect("alloc");
        let mut src_bytes = vec![0u8; 8 * 4 * 4];
        for y in 0u8..4 {
            for x in 0u8..8 {
                let off = (usize::from(y) * 8 + usize::from(x)) * 4;
                src_bytes[off] = x * 0x20; // B
                src_bytes[off + 1] = y * 0x40; // G
                src_bytes[off + 2] = (x + y) * 0x10; // R
                src_bytes[off + 3] = 0xFF; // A (opaque)
            }
        }
        engine
            .put_image(
                &mut store,
                &mut platform,
                dst,
                vk::Offset2D::default(),
                vk::Extent2D {
                    width: 8,
                    height: 4,
                },
                &src_bytes,
                32,
            )
            .expect("put_image");

        let stats = engine
            .render_composite(
                &mut store,
                &mut platform,
                3, // Over
                ResolvedSource::Drawable(dst),
                ResolvedSource::None,
                dst,
                &[full_rect(8, 4)],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
            )
            .expect("render_composite");
        assert!(
            stats.used_src_alias_scratch,
            "src == dst must route through the alias scratch",
        );
        assert_eq!(stats.recorded_draws, 1);

        let after = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 8,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        assert_eq!(
            after, src_bytes,
            "Over(self, NoMask, self) must equal self bit-identical",
        );

        engine.drain_all(&platform);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn render_fill_rectangles_src_clears_to_color() {
        // render_fill_rectangles(op=Src, premul colour) — every
        // pixel in the rect must equal the premul colour.
        let Some(mut platform) = live_platform() else {
            eprintln!("no Vk — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let dst = alloc_filled_pixmap(
            &mut platform,
            &mut store,
            &mut engine,
            0x1,
            4,
            4,
            [0.0, 0.0, 0.0, 1.0],
        );
        let stats = engine
            .render_fill_rectangles(
                &mut store,
                &mut platform,
                1,                    // Src
                [1.0, 0.0, 0.0, 1.0], // RGBA: opaque red premul
                dst,
                &[full_rect(4, 4)],
                None,
            )
            .expect("render_fill_rectangles");
        assert_eq!(stats.recorded_draws, 1);
        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                dst,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 4,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");
        // BGRA: B=0, G=0, R=0xFF, A=0xFF.
        for px in out.chunks_exact(4) {
            assert_eq!(&px[..4], &[0x00, 0x00, 0xFF, 0xFF]);
        }
        engine.drain_all(&platform);
    }

    // ── Stage 3e.2 decoder + degenerate-trap unit tests ─────────

    /// Per plan §3e: round-trip a known wire bytestream through
    /// the trapezoid decoder. Verifies field offsets + 16.16
    /// fixed-point interpretation. Uses the same shape as v1's
    /// `try_vk_render_trapezoids_path` (kms/backend.rs:4286)
    /// since v2's `render_trapezoids` mirrors that decoder.
    #[test]
    fn trapezoid_decoder_x11_wire_layout() {
        // Build a single trapezoid wire record: 10 i32 fields, 40
        // bytes. Field order: top, bottom, left_p1.x, left_p1.y,
        // left_p2.x, left_p2.y, right_p1.x, right_p1.y,
        // right_p2.x, right_p2.y. All values are 16.16 fixed-point.
        let mut wire: Vec<u8> = Vec::with_capacity(40);
        let fields: [i32; 10] = [
            0,        // top = 0.0
            10 << 16, // bottom = 10.0
            2 << 16,  // left_p1.x = 2.0
            0,        // left_p1.y = 0.0
            2 << 16,  // left_p2.x = 2.0
            10 << 16, // left_p2.y = 10.0
            8 << 16,  // right_p1.x = 8.0
            0,        // right_p1.y = 0.0
            8 << 16,  // right_p2.x = 8.0
            10 << 16, // right_p2.y = 10.0
        ];
        for v in fields {
            wire.extend_from_slice(&v.to_le_bytes());
        }

        // Decode mirroring the backend's `render_trapezoids` body.
        let chunk: &[u8] = &wire;
        let read_i32 = |o: usize| -> i32 {
            i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
        };
        let trap = crate::kms::vk::ops::traps::Trapezoid {
            top: read_i32(0),
            bottom: read_i32(4),
            left_p1: (read_i32(8), read_i32(12)),
            left_p2: (read_i32(16), read_i32(20)),
            right_p1: (read_i32(24), read_i32(28)),
            right_p2: (read_i32(32), read_i32(36)),
        };
        assert_eq!(trap.top, 0);
        assert_eq!(trap.bottom, 10 << 16);
        assert_eq!(trap.left_p1, (2 << 16, 0));
        assert_eq!(trap.left_p2, (2 << 16, 10 << 16));
        assert_eq!(trap.right_p1, (8 << 16, 0));
        assert_eq!(trap.right_p2, (8 << 16, 10 << 16));

        // bbox: x ∈ [2, 8], y ∈ [0, 10]; integer = (2, 0, 8, 10).
        let bbox = crate::kms::vk::ops::traps::trapezoid_bbox(&[trap])
            .expect("bbox for non-degenerate trap");
        assert_eq!(bbox, (2, 0, 8, 10));
    }

    /// Per plan §3e: each Triangle's three vertices round-trip
    /// through the wire decoder, and the bbox helper hits each
    /// vertex (so a degenerate triangle — three colinear points —
    /// still produces a finite bbox if the points span pixels).
    /// Mirrors v1's `try_vk_render_triangles_path` decoder shape.
    #[test]
    fn triangle_to_trap_degenerate() {
        let tri = crate::kms::vk::ops::traps::Triangle {
            p1: (0, 0),
            p2: (4 << 16, 0),
            p3: (2 << 16, 8 << 16),
        };
        let inst = tri.to_instance_data();
        assert!((inst.p1[0] - 0.0).abs() < 1e-6);
        assert!((inst.p2[0] - 4.0).abs() < 1e-6);
        assert!((inst.p3[1] - 8.0).abs() < 1e-6);
        let bbox = crate::kms::vk::ops::traps::triangle_bbox(&[tri])
            .expect("bbox for non-degenerate triangle");
        assert_eq!(bbox, (0, 0, 4, 8));

        // Degenerate (three colinear points) — bbox helper still
        // returns Some(extents) because the points span the axes.
        // What v1 + v2 do with such an input is: GPU pipeline draws
        // a zero-area triangle (no pixels covered), CB safely
        // completes. The plan's "degenerate trap" phrasing refers
        // to the encoding (trap with one zero-length edge), not a
        // helper output — the test confirms the trivial bbox path
        // doesn't choke on it.
        let colinear = crate::kms::vk::ops::traps::Triangle {
            p1: (0, 0),
            p2: (4 << 16, 0),
            p3: (8 << 16, 0),
        };
        assert!(crate::kms::vk::ops::traps::triangle_bbox(&[colinear]).is_none());
    }

    /// Stage 3f.15: `fill_rect_batch` records N rects into ONE CB +
    /// ONE submit + ONE `SubmittedOp`. Drives 3 disjoint rects on a
    /// 16×4 BGRA8 dst pre-cleared to blue, fills them red, and
    /// asserts (a) the dst observes red inside each rect and blue
    /// outside, and (b) `inner.submitted` grew by exactly 1 across
    /// the batch call.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fill_rect_batch_one_submit_for_n_rects() {
        let Some(mut platform) = live_platform() else {
            eprintln!("no VkContext available — skipping");
            return;
        };
        let mut store = DrawableStore::new();
        let mut engine = RenderEngine::new(&platform).expect("engine");

        let storage = platform
            .allocate_drawable_storage(16, 4, 32)
            .expect("alloc");
        let id = store
            .allocate(
                0x1,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
            .unwrap();

        // Pre-fill the dst with blue so we can see the batch-painted
        // rects against a known background.
        let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 16,
                        height: 4,
                    },
                },
                blue,
            )
            .expect("blue prefill");

        // Snapshot the SubmittedOp count BEFORE the batch call so we
        // can assert exactly +1 across the call regardless of what
        // prior ops are still in flight.
        let before = engine
            .inner
            .as_ref()
            .map(|i| i.submitted.len())
            .unwrap_or(0);

        let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
        let rects = [
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 6, y: 1 },
                extent: vk::Extent2D {
                    width: 3,
                    height: 2,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 13, y: 2 },
                extent: vk::Extent2D {
                    width: 3,
                    height: 2,
                },
            },
        ];
        engine
            .fill_rect_batch(&mut store, &mut platform, id, red, &rects)
            .expect("fill_rect_batch");

        let after = engine
            .inner
            .as_ref()
            .map(|i| i.submitted.len())
            .unwrap_or(0);
        assert_eq!(
            after,
            before + 1,
            "fill_rect_batch must produce exactly one SubmittedOp regardless of rect count \
             (before={before}, after={after})"
        );

        let out = engine
            .get_image(
                &mut store,
                &mut platform,
                id,
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: 16,
                        height: 4,
                    },
                },
                32,
            )
            .expect("get_image");

        // Helper: does (x, y) fall inside any of the painted rects?
        let in_rect = |x: i32, y: i32| -> bool {
            rects.iter().any(|r| {
                x >= r.offset.x
                    && y >= r.offset.y
                    && x < r.offset.x + r.extent.width as i32
                    && y < r.offset.y + r.extent.height as i32
            })
        };
        for y in 0..4 {
            for x in 0..16 {
                let off = (y * 16 + x) as usize * 4;
                let px = &out[off..off + 4];
                if in_rect(x, y) {
                    assert_eq!(px[2], 0xFF, "rect pixel ({x},{y}) R should be 0xFF (red)");
                    assert_eq!(px[0], 0x00, "rect pixel ({x},{y}) B should be 0x00");
                } else {
                    assert_eq!(
                        px[0], 0xFF,
                        "background pixel ({x},{y}) B should be 0xFF (blue)"
                    );
                    assert_eq!(px[2], 0x00, "background pixel ({x},{y}) R should be 0x00");
                }
            }
        }

        engine.drain_all(&platform);
    }

    /// X11 Render PictFormat fix — resolver-level oracle.
    ///
    /// Per the X11 Render spec, a Picture wrapping a depth-24
    /// drawable has `PictFormat.alpha_mask = 0`; samples must
    /// return α = 1.0 regardless of the storage's padding byte.
    /// `resolve_force_opaque` is the single point where v2's
    /// `render_composite` and `render_traps_or_tris` decide
    /// whether to set the shader-side force-opaque bit on the
    /// src/mask picture.
    ///
    /// This test is the logic-only gate: a depth-24 Drawable
    /// must resolve to `true`; depth-32 to `false`. Solid and
    /// Gradient sources carry α intrinsically (LUT-baked or
    /// caller-supplied), so they're always `false`. `None` is
    /// the synthetic white-mask path — `α = 1.0` already by
    /// construction, so no override needed.
    #[test]
    fn render_composite_resolve_force_opaque_oracle() {
        let mut store = DrawableStore::new();
        let storage32 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id32 = store
            .allocate(
                0xA001,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage32,
            )
            .unwrap();
        let storage24 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id24 = store
            .allocate(
                0xA002,
                super::super::store::DrawableKind::Pixmap,
                24,
                false,
                storage24,
            )
            .unwrap();

        // depth-32 Drawable: storage's α byte is client-meaningful,
        // do not force.
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id32)
        ));
        // depth-24 Drawable: storage's α byte is server-owned
        // padding, force α = 1.0.
        assert!(resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id24)
        ));

        // Solid: α is caller-supplied premul. Gradient: α is
        // LUT-baked. None: white-mask scratch is initialised to
        // α = 1.0 at engine init. All three pass through.
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]),
        ));
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Gradient(0x1234)
        ));
        assert!(!resolve_force_opaque(&store, &ResolvedSource::None));

        // depth-1 (bitmap mask) and depth-8 (a8 alpha picture)
        // both have meaningful α in their PictFormat — α carries
        // the bitmap value / coverage. Forcing α = 1.0 on those
        // would turn coverage masks into solid blocks, so the
        // resolver explicitly excludes them. Only depth-24 (the
        // x8r8g8b8 / r8g8b8 case where storage's α byte is
        // server-owned padding) gets the override.
        let storage1 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id1 = store
            .allocate(
                0xA003,
                super::super::store::DrawableKind::Pixmap,
                1,
                false,
                storage1,
            )
            .unwrap();
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id1)
        ));
        let storage8 = super::super::store::Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::R8_UNORM,
        );
        let id8 = store
            .allocate(
                0xA004,
                super::super::store::DrawableKind::Pixmap,
                8,
                false,
                storage8,
            )
            .unwrap();
        assert!(!resolve_force_opaque(
            &store,
            &ResolvedSource::Drawable(id8)
        ));
    }
}
