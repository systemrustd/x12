//! Graphics pipeline + sampler + descriptor set for the glyph-render
//! pass (sub-phase 4.1.4.5).
//!
//! Mirrors [`pipeline::CompositorPipeline`](super::pipeline) very
//! closely; the differences are the shaders (`text.vert.spv` /
//! `text.frag.spv`), the descriptor binding (single combined
//! image-sampler at binding 0 sampled by the fragment shader as an
//! `R8_UNORM` alpha mask), and the push-constant layout
//! (vec2 quintuple + vec4 foreground = 56 bytes).
//!
//! The atlas image view is bound *once* at pipeline construction —
//! [`GlyphAtlas`](super::glyph::GlyphAtlas) is a single
//! backend-wide image whose handle never changes for the rest of
//! the session — so a single descriptor set serves every
//! [`record_text_run`](super::ops::text::record_text_run) call. No
//! per-frame descriptor pool reset like the compositor pipeline.
//!
//! Sampler is `NEAREST`. Glyph quads in the atlas are tightly
//! packed (no padding), so `LINEAR` filtering would pull in
//! adjacent-glyph pixels at the edges and produce visible
//! bleeding artefacts (seen on bare-metal xterm runs). FreeType
//! already produces anti-aliased alpha bitmaps, so the sampler
//! doesn't need to do further blending — `NEAREST` lookup is
//! exactly the right thing.
//!
//! Color attachment format is the per-window mirror format
//! (`B8G8R8A8_UNORM` — every server-allocated mirror uses this
//! since 4.1.3.2). The pipeline is built once per backend
//! lifetime against that format.

use std::sync::Arc;

use ash::vk;

use super::{device::VkContext, glyph::GlyphAtlas};

const VERTEX_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.vert.spv"));
const FRAGMENT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.frag.spv"));

/// Push constants for the glyph quad shader. 56 bytes — well
/// within the 128-byte minimum `maxPushConstantsSize`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TextPushConsts {
    pub dst_origin: [f32; 2],
    pub dst_size: [f32; 2],
    pub viewport: [f32; 2],
    pub src_origin: [f32; 2],
    pub src_size: [f32; 2],
    /// RGB foreground, alpha set to 1.0 by the caller.
    pub foreground: [f32; 4],
}

impl TextPushConsts {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` with f32 fields, no padding (asserted
        // below). Any byte pattern is a valid f32; round-tripping
        // through `&[u8]` is safe.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

const _: () = assert!(std::mem::size_of::<TextPushConsts>() == 56);

pub struct TextPipeline {
    vk: Arc<VkContext>,
    pub pipeline: vk::Pipeline,
    pub pipeline_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,
    pub color_format: vk::Format,
}

#[derive(Debug, thiserror::Error)]
pub enum TextPipelineError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("text shader SPIR-V from build.rs is malformed (length not multiple of 4): {0} bytes")]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for TextPipelineError {
    fn from(r: vk::Result) -> Self {
        TextPipelineError::Vk(r)
    }
}

impl TextPipeline {
    pub fn new(
        vk: Arc<VkContext>,
        color_format: vk::Format,
        atlas: &GlyphAtlas,
    ) -> Result<Self, TextPipelineError> {
        let device = &vk.device;

        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .min_lod(0.0)
            .max_lod(0.0);
        let sampler = unsafe { device.create_sampler(&sampler_info, None)? };

        let dsl_bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let dsl_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&dsl_bindings);
        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&dsl_info, None) } {
                Ok(d) => d,
                Err(e) => {
                    unsafe { device.destroy_sampler(sampler, None) };
                    return Err(e.into());
                }
            };

        let set_layouts = [descriptor_set_layout];
        let push_const_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<TextPushConsts>() as u32)];
        let pl_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_const_ranges);
        let pipeline_layout = match unsafe { device.create_pipeline_layout(&pl_info, None) } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };

        let vert_module = create_shader_module(device, VERTEX_SPV)?;
        let frag_module = match create_shader_module(device, FRAGMENT_SPV) {
            Ok(m) => m,
            Err(e) => {
                unsafe {
                    device.destroy_shader_module(vert_module, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e);
            }
        };

        let entry = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert_module)
                .name(entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag_module)
                .name(entry),
        ];

        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // Premultiplied src-over.
        let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend_attachments);
        let dynamic_state_array = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_state_array);
        let color_formats = [color_format];
        let mut rendering_info =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);

        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_state)
            .layout(pipeline_layout)
            .push_next(&mut rendering_info);

        let pipeline = match unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
        } {
            Ok(ps) => ps[0],
            Err((_, e)) => {
                unsafe {
                    device.destroy_shader_module(vert_module, None);
                    device.destroy_shader_module(frag_module, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
        unsafe {
            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);
        }

        // Single descriptor set bound to the atlas. Atlas image
        // view never changes; we never reset or re-allocate.
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = match unsafe { device.create_descriptor_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_pipeline(pipeline, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
        let alloc_layouts = [descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&alloc_layouts);
        let descriptor_set = match unsafe { device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                unsafe {
                    device.destroy_descriptor_pool(descriptor_pool, None);
                    device.destroy_pipeline(pipeline, None);
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(atlas.image_view())
            .sampler(sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { device.update_descriptor_sets(&writes, &[]) };

        Ok(Self {
            vk,
            pipeline,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            descriptor_pool,
            descriptor_set,
            color_format,
        })
    }
}

impl Drop for TextPipeline {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk
                .device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.vk.device.destroy_pipeline(self.pipeline, None);
            self.vk
                .device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.vk
                .device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.vk.device.destroy_sampler(self.sampler, None);
        }
    }
}

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, TextPipelineError> {
    if !spv_bytes.len().is_multiple_of(4) {
        return Err(TextPipelineError::SpirvUnaligned(spv_bytes.len()));
    }
    let mut code: Vec<u32> = Vec::with_capacity(spv_bytes.len() / 4);
    for chunk in spv_bytes.chunks_exact(4) {
        code.push(u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    Ok(unsafe { device.create_shader_module(&info, None)? })
}
