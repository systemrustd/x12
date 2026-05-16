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

use std::{collections::VecDeque, ptr::NonNull, sync::Arc};

use ash::vk;

use super::{
    glyph_atlas::V2GlyphAtlas,
    platform::{FenceTicket, PlatformBackend},
    store::{DrawableId, DrawableStore},
};
use crate::kms::vk::{
    device::VkContext,
    glyph::{AtlasEntry, GlyphKey},
    ops::text::{TextAtlas, TextGlyph, TextRunTarget, record_text_run},
    text_pipeline::TextPipeline,
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
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
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
                glyph_atlas: None,
                text_pipeline: None,
                atlas_last_upload_ticket: None,
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
        }
    }

    /// Count of in-flight submits awaiting retirement. Tests use
    /// this to assert the lifecycle book-keeping.
    pub(crate) fn pending_count(&self) -> usize {
        self.inner.as_ref().map(|i| i.submitted.len()).unwrap_or(0)
    }

    // ── Op: fill_rect ───────────────────────────────────────────

    /// Fill `rect` in `target`'s storage with `color` (RGBA float).
    /// `vkCmdClearAttachments` inside an active render pass — no
    /// pipeline / shader / descriptor set needed; matches v1's
    /// `fill::record_fill_rectangles` choice.
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
        if rect.extent.width == 0 || rect.extent.height == 0 {
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
        let extent = drawable.storage.extent;
        let image_view = drawable.storage.image_view;

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

            let attachments = [vk::ClearAttachment::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .color_attachment(0)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32: color },
                })];
            let clear_rects = [vk::ClearRect::default()
                .rect(clamp_rect(rect, extent))
                .base_array_layer(0)
                .layer_count(1)];
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
        store.damage(target, rect);
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
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
        });

        Ok(stats)
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
        let storage = platform
            .allocate_drawable_storage(w, h, 32)
            .expect("alloc storage");
        store
            .allocate(
                xid,
                super::super::store::DrawableKind::Pixmap,
                32,
                false,
                storage,
            )
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

        let id = alloc_drawable_3a(&platform, &mut store, 0x1, 64, 32);
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
        let rects: Vec<vk::Rect2D> = d.protocol_damage.rects().to_vec();
        assert!(!rects.is_empty(), "protocol damage should be set");
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
}
