//! RENDER `Composite` + `FillRectangles` recorder (sub-phase
//! 4.1.4.6, commit 1).
//!
//! Records a per-op pipelined draw into the destination mirror.
//! The caller has already:
//!   * picked a [`StdPictOp`] from the request and resolved the
//!     pipeline via [`RenderPipelineCache::get`],
//!   * resolved the source — either a Drawable's mirror (sampled
//!     directly) or a `SolidFill` whose colour we cleared into a
//!     backend-shared 1×1 image,
//!   * allocated a descriptor set bound to the source view + the
//!     pipeline's linear sampler,
//!   * computed clip scissors (Region clip) — empty Vec means
//!     "no clip" so we use a full-mirror scissor.
//!
//! The recorder transitions the destination mirror through
//! `SHADER_READ_ONLY_OPTIMAL → COLOR_ATTACHMENT_OPTIMAL → back`,
//! `cmd_begin_rendering` with `LOAD/STORE`, sets the per-rect
//! viewport+scissor, binds the pipeline + descriptor set, push-
//! constants the dst+src UV rect, and `cmd_draw(4, 1)` per call.
//!
//! Mask + alpha_map + transform + component_alpha + R8 mask
//! support layer in across the follow-up commits in this slot.

use ash::vk;

use crate::kms::vk::{device::VkContext, render_pipeline::RenderPushConsts, target::DrawableImage};

/// Minimal paint-target surface that [`record_render_composite`]
/// reads / mutates. Lets the recorder operate against either v1's
/// `DrawableImage` (per-window mirror) or v2's `Drawable` storage
/// (via an adapter) without depending on either type's full shape.
/// Stage 3 plan §"Cross-cutting" §1.
pub trait CompositeTarget {
    fn vk_image(&self) -> vk::Image;
    fn vk_image_view(&self) -> vk::ImageView;
    fn extent(&self) -> vk::Extent2D;
    fn current_layout(&self) -> vk::ImageLayout;
    fn set_current_layout(&mut self, layout: vk::ImageLayout);
}

impl CompositeTarget for DrawableImage {
    fn vk_image(&self) -> vk::Image {
        self.vk_image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.vk_image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout()
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.set_current_layout(layout);
    }
}

/// One Composite request sub-rect. `src_x`/`src_y` and
/// `mask_x`/`mask_y` are the source / mask space pixel offsets
/// corresponding to `dst_x`/`dst_y`; the fragment shader applies the
/// per-picture affine transform on top of those.
pub struct CompositeRect {
    pub src_x: i32,
    pub src_y: i32,
    pub mask_x: i32,
    pub mask_y: i32,
    pub dst_x: i32,
    pub dst_y: i32,
    pub width: u32,
    pub height: u32,
}

/// Affine 2×3 transform packed for the shader (rows of an X11 RENDER
/// 3×3 matrix dropped to two affine rows). For the identity transform,
/// the shader collapses to source-pixel = dst-offset + origin so the
/// "no transform" path is `IDENTITY`.
#[derive(Debug, Clone, Copy)]
pub struct AffineXform {
    pub row0: [f32; 4], // (a, b, tx, _)
    pub row1: [f32; 4], // (c, d, ty, _)
}

impl AffineXform {
    pub const IDENTITY: AffineXform = AffineXform {
        row0: [1.0, 0.0, 0.0, 0.0],
        row1: [0.0, 1.0, 0.0, 0.0],
    };
}

/// Per-Composite-call attribute bundle.
pub struct CompositeAttrs {
    pub src_extent: ash::vk::Extent2D,
    pub mask_extent: ash::vk::Extent2D,
    pub src_repeat: i32,
    pub mask_repeat: i32,
    pub src_xform: AffineXform,
    pub mask_xform: AffineXform,
}

/// Record `rects` worth of Composite draws into `dst`. The
/// descriptor set has `src_tex` at binding 0 and `mask_tex` at
/// binding 1; both must already be in `SHADER_READ_ONLY_OPTIMAL`
/// (caller arranges that — Drawable mirrors are already there
/// from the last composite, SolidFill scratch is transitioned by
/// the caller's clear sequence, the white-mask scratch is
/// transitioned once at backend init and stays put).
#[allow(clippy::too_many_arguments)]
pub fn record_render_composite<T: CompositeTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst: &mut T,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set: vk::DescriptorSet,
    attrs: &CompositeAttrs,
    rects: &[CompositeRect],
    clip_scissor: vk::Rect2D,
) -> Result<(), vk::Result> {
    let extent = dst.extent();
    if rects.is_empty() || extent.width == 0 || extent.height == 0 {
        return Ok(());
    }

    let device = &vk.device;
    let old_layout = dst.current_layout();
    let dst_image = dst.vk_image();
    let dst_view = dst.vk_image_view();

    let to_color = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
        )
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(dst_image)
        .subresource_range(color_subresource_range())];
    let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_color_dep) };

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(dst_view)
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
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    // Clamp the caller's clip scissor to the render area. The
    // unclipped path passes `Extent2D { width: u32::MAX, height:
    // u32::MAX }` (build_render_composite_inputs) which trips
    // Vulkan validation (`offset.x + extent.width` overflow), and
    // some drivers (lavapipe) silently drop the draw when that
    // happens.
    let scissor = [vk::Rect2D {
        offset: clip_scissor.offset,
        extent: vk::Extent2D {
            width: clip_scissor.extent.width.min(
                extent
                    .width
                    .saturating_sub(clip_scissor.offset.x.max(0) as u32),
            ),
            height: clip_scissor.extent.height.min(
                extent
                    .height
                    .saturating_sub(clip_scissor.offset.y.max(0) as u32),
            ),
        },
    }];

    unsafe {
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        crate::vk_count!(cmd_set_scissor);
        device.cmd_set_scissor(cb, 0, &scissor);
        crate::vk_count!(cmd_bind_pipeline);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        crate::vk_count!(cmd_bind_descriptor_sets);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[descriptor_set],
            &[],
        );

        let dst_vp = [extent.width as f32, extent.height as f32];
        let src_extent_px = [
            attrs.src_extent.width as f32,
            attrs.src_extent.height as f32,
        ];
        let mask_extent_px = [
            attrs.mask_extent.width as f32,
            attrs.mask_extent.height as f32,
        ];
        for r in rects {
            let pc = RenderPushConsts {
                dst_origin: [r.dst_x as f32, r.dst_y as f32],
                dst_size: [r.width as f32, r.height as f32],
                viewport: dst_vp,
                src_origin: [r.src_x as f32, r.src_y as f32],
                mask_origin: [r.mask_x as f32, r.mask_y as f32],
                src_extent: src_extent_px,
                mask_extent: mask_extent_px,
                repeat_modes: [attrs.src_repeat, attrs.mask_repeat],
                src_xform_row0: attrs.src_xform.row0,
                src_xform_row1: attrs.src_xform.row1,
                mask_xform_row0: attrs.mask_xform.row0,
                mask_xform_row1: attrs.mask_xform.row1,
            };
            crate::vk_count!(cmd_push_constants);
            device.cmd_push_constants(
                cb,
                pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                pc.as_bytes(),
            );
            crate::vk_count!(cmd_draw);
            device.cmd_draw(cb, 4, 1, 0, 0);
        }

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);
    }

    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(dst_image)
        .subresource_range(color_subresource_range())];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    crate::vk_count!(cmd_pipeline_barrier2);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    dst.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
