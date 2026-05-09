//! Text-run drawing op (sub-phase 4.1.4.5).
//!
//! Per-glyph quad draws into the target mirror, sampling the
//! shared [`GlyphAtlas`](super::super::glyph::GlyphAtlas) via the
//! [`TextPipeline`](super::super::text_pipeline::TextPipeline) and
//! premultiplied src-over blending. The glyphs the caller passes
//! in have already been packed into the atlas (the caller invokes
//! `GlyphAtlas::intern` for each glyph in the run before
//! recording; misses fall back to the pixman path).

use ash::vk;

use crate::kms::vk::{
    device::VkContext,
    glyph::{AtlasEntry, GlyphAtlas},
    target::DrawableImage,
    text_pipeline::{TextPipeline, TextPushConsts},
};

/// One glyph's placement within a text run. `dst_x` / `dst_y` are
/// the top-left of the glyph quad in mirror pixel coords (caller
/// has already applied `entry.pen_left` and `entry.pen_top`).
pub struct TextGlyph {
    pub entry: AtlasEntry,
    pub dst_x: i32,
    pub dst_y: i32,
}

/// Record a text run into `target`. `foreground` is the GC's RGB
/// foreground in `[0..1]`; the shader multiplies it by the
/// sampled atlas alpha. Mirror is left in
/// `SHADER_READ_ONLY_OPTIMAL` on return.
pub fn record_text_run(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    target: &mut DrawableImage,
    atlas: &GlyphAtlas,
    pipeline: &TextPipeline,
    glyphs: &[TextGlyph],
    foreground: [f32; 4],
) -> Result<(), vk::Result> {
    if glyphs.is_empty() || target.extent.width == 0 || target.extent.height == 0 {
        return Ok(());
    }

    let device = &vk.device;
    let old_layout = target.current_layout();

    let to_color = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
        )
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(target.vk_image)
        .subresource_range(color_subresource_range())];
    let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_color_dep) };

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: target.extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(target.vk_image_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);

    let atlas_extent = atlas.extent();
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: target.extent.width as f32,
        height: target.extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let scissor = [render_area];

    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        device.cmd_set_viewport(cb, 0, &viewport);
        device.cmd_set_scissor(cb, 0, &scissor);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline.pipeline);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline.pipeline_layout,
            0,
            &[pipeline.descriptor_set],
            &[],
        );

        for g in glyphs {
            if g.entry.w == 0 || g.entry.h == 0 {
                continue;
            }
            let pc = TextPushConsts {
                dst_origin: [g.dst_x as f32, g.dst_y as f32],
                dst_size: [g.entry.w as f32, g.entry.h as f32],
                viewport: [target.extent.width as f32, target.extent.height as f32],
                src_origin: [
                    g.entry.atlas_x as f32 / atlas_extent.width as f32,
                    g.entry.atlas_y as f32 / atlas_extent.height as f32,
                ],
                src_size: [
                    g.entry.w as f32 / atlas_extent.width as f32,
                    g.entry.h as f32 / atlas_extent.height as f32,
                ],
                foreground,
            };
            device.cmd_push_constants(
                cb,
                pipeline.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                pc.as_bytes(),
            );
            device.cmd_draw(cb, 4, 1, 0, 0);
        }

        device.cmd_end_rendering(cb);
    }

    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(target.vk_image)
        .subresource_range(color_subresource_range())];
    let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

    target.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    Ok(())
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
