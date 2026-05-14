//! Dst pixel readback scratch for Disjoint/Conjoint RENDER ops.
//!
//! The X RENDER Disjoint and Conjoint operator families are defined
//! by Fa(As, Ad) and Fb(As, Ad) functions that involve `min`/`max` of
//! `(1 - Ad) / As`-style expressions — these can't be expressed as
//! fixed-function `VkPipelineColorBlendAttachmentState` factors.
//! Instead the fragment shader (`render.frag.glsl` `MODE=1`) computes
//! the blend manually, which means it needs to *read* the existing dst
//! pixel as a sampled texture before producing the new colour.
//!
//! Vulkan can't sample an image while it's bound as a color
//! attachment in the same draw without `dynamic_rendering_local_read`
//! (Vulkan 1.4). We use the broadly-supported workaround: copy the
//! dst into a scratch image just before recording the draw, bind the
//! scratch as `binding 2`, and let the pipeline write to the
//! attachment as usual.
//!
//! This module keeps one scratch per dst format (BGRA + R8). They
//! grow power-of-two on demand and are reused across calls. R8
//! variants expose a swizzled view (`a = R`) so the shader sees
//! `(0, 0, 0, alpha)` — matching the `mask_image_view` convention
//! and X RENDER's "rgb defaults to 0 for alpha-only pictures".

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;
use crate::kms::scheduler::paint_batch::BatchResource;

#[derive(Debug, thiserror::Error)]
pub enum DstReadbackError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no memory type matches dst readback requirements")]
    NoMemoryType,
}

impl From<vk::Result> for DstReadbackError {
    fn from(r: vk::Result) -> Self {
        DstReadbackError::Vk(r)
    }
}

/// 5-T4: wraps the just-replaced per-format scratch (image + views +
/// memory) so the scheduler can release it after the in-flight batch
/// retires. The new image has already been installed on `DstReadback`
/// by the time this is constructed.
#[derive(Debug)]
struct RetiredDstReadbackImage {
    image: vk::Image,
    view: vk::ImageView,
    no_alpha_view: Option<vk::ImageView>,
    memory: vk::DeviceMemory,
}

impl BatchResource for RetiredDstReadbackImage {
    fn release(self: Box<Self>, vk: &VkContext) {
        unsafe {
            if let Some(v) = self.no_alpha_view {
                vk.device.destroy_image_view(v, None);
            }
            vk.device.destroy_image_view(self.view, None);
            vk.device.destroy_image(self.image, None);
            vk.device.free_memory(self.memory, None);
        }
    }
}

struct ReadbackImage {
    image: vk::Image,
    view: vk::ImageView,
    /// Swizzled view (`a = ONE`) for BGRA scratches sampled by a
    /// shader on behalf of a picture format with no alpha mask
    /// (r8g8b8 / x8r8g8b8). The underlying image is the same; only
    /// the view's component swizzle changes. `None` until first use.
    /// Not used for R8 scratches (R8 is always alpha-only).
    no_alpha_view: Option<vk::ImageView>,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    current_layout: vk::ImageLayout,
}

pub struct DstReadback {
    vk: Arc<VkContext>,
    bgra: Option<ReadbackImage>,
    r8: Option<ReadbackImage>,
}

impl DstReadback {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            bgra: None,
            r8: None,
        }
    }

    /// 5-T4: like the pre-Phase-5 `ensure` but returns the old
    /// per-format image wrapped as a `BatchResource` for the caller
    /// to defer-release through the scheduler. Returns `Ok(None)`
    /// if no grow was needed (slot is fine, scratch unchanged).
    ///
    /// Allocation-first ordering: if `allocate` fails the scratch is
    /// untouched and the caller sees `Err`. Past the successful
    /// `allocate` the function MUST NOT fail.
    pub fn ensure_returning_old(
        &mut self,
        format: vk::Format,
        width: u32,
        height: u32,
    ) -> Result<Option<Box<dyn BatchResource>>, DstReadbackError> {
        let slot = match format {
            vk::Format::B8G8R8A8_UNORM => &mut self.bgra,
            vk::Format::R8_UNORM => &mut self.r8,
            _ => return Err(DstReadbackError::NoMemoryType),
        };
        if let Some(img) = slot.as_ref()
            && img.extent.width >= width
            && img.extent.height >= height
        {
            return Ok(None);
        }
        let new_extent = match slot.as_ref() {
            Some(img) => vk::Extent2D {
                width: img.extent.width.max(width).next_power_of_two().max(64),
                height: img.extent.height.max(height).next_power_of_two().max(64),
            },
            None => vk::Extent2D {
                width: width.next_power_of_two().max(64),
                height: height.next_power_of_two().max(64),
            },
        };
        let new_img = allocate(&self.vk, format, new_extent)?;
        // Allocation succeeded — past here the function MUST NOT fail.
        let retired = slot.take().map(|old| {
            Box::new(RetiredDstReadbackImage {
                image: old.image,
                view: old.view,
                no_alpha_view: old.no_alpha_view,
                memory: old.memory,
            }) as Box<dyn BatchResource>
        });
        *slot = Some(new_img);
        Ok(retired)
    }

    /// Sampleable view for the per-format scratch.
    ///
    /// When `dst_has_alpha == false` (r8g8b8 / x8r8g8b8 destination),
    /// the BGRA scratch returns a swizzled view (`a = ONE`) so the
    /// shader's `texelFetch(...).a` matches the X RENDER convention
    /// for no-alpha picture formats. R8 scratches ignore the flag —
    /// R8 (a8 picture) is alpha-only by definition; the regular view
    /// already swizzles `a = R`.
    pub fn view(
        &mut self,
        format: vk::Format,
        dst_has_alpha: bool,
    ) -> Result<Option<vk::ImageView>, vk::Result> {
        match format {
            vk::Format::B8G8R8A8_UNORM => {
                let Some(img) = self.bgra.as_mut() else {
                    return Ok(None);
                };
                if dst_has_alpha {
                    return Ok(Some(img.view));
                }
                if let Some(v) = img.no_alpha_view {
                    return Ok(Some(v));
                }
                let view_info = vk::ImageViewCreateInfo::default()
                    .image(img.image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .components(vk::ComponentMapping {
                        r: vk::ComponentSwizzle::IDENTITY,
                        g: vk::ComponentSwizzle::IDENTITY,
                        b: vk::ComponentSwizzle::IDENTITY,
                        a: vk::ComponentSwizzle::ONE,
                    })
                    .subresource_range(color_subresource_range());
                let v = unsafe { self.vk.device.create_image_view(&view_info, None)? };
                img.no_alpha_view = Some(v);
                Ok(Some(v))
            }
            vk::Format::R8_UNORM => Ok(self.r8.as_ref().map(|i| i.view)),
            _ => Ok(None),
        }
    }

    /// Record a copy from `dst_image` (currently in `dst_layout`) into
    /// the per-format scratch image, then transition the scratch into
    /// `SHADER_READ_ONLY_OPTIMAL` and the dst back into `dst_layout`.
    /// Caller must have called `ensure_returning_old` for the dst
    /// format/extent.
    pub fn record_copy_from(
        &mut self,
        cb: vk::CommandBuffer,
        dst_image: vk::Image,
        dst_layout: vk::ImageLayout,
        format: vk::Format,
        copy_extent: vk::Extent2D,
    ) {
        let device = &self.vk.device;
        let scratch = match format {
            vk::Format::B8G8R8A8_UNORM => self
                .bgra
                .as_mut()
                .expect("ensure_returning_old() not called"),
            vk::Format::R8_UNORM => self.r8.as_mut().expect("ensure_returning_old() not called"),
            _ => unreachable!("ensure_returning_old() rejected this format"),
        };

        // Transition: dst → TRANSFER_SRC, scratch → TRANSFER_DST.
        let pre = [
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::empty())
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(dst_layout)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(dst_image)
                .subresource_range(color_subresource_range()),
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::empty())
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(scratch.current_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(scratch.image)
                .subresource_range(color_subresource_range()),
        ];
        let pre_dep = vk::DependencyInfo::default().image_memory_barriers(&pre);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &pre_dep) };

        let region = [vk::ImageCopy::default()
            .src_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .dst_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .extent(vk::Extent3D {
                width: copy_extent.width,
                height: copy_extent.height,
                depth: 1,
            })];
        unsafe {
            crate::vk_count!(cmd_copy_image);
            device.cmd_copy_image(
                cb,
                dst_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                scratch.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            );
        }

        // Transition: scratch → SHADER_READ, dst → original layout.
        let post = [
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(scratch.image)
                .subresource_range(color_subresource_range()),
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_access_mask(vk::AccessFlags2::empty())
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(dst_layout)
                .image(dst_image)
                .subresource_range(color_subresource_range()),
        ];
        let post_dep = vk::DependencyInfo::default().image_memory_barriers(&post);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &post_dep) };

        scratch.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }
}

impl Drop for DstReadback {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            for img in [self.bgra.take(), self.r8.take()].into_iter().flatten() {
                if let Some(v) = img.no_alpha_view {
                    self.vk.device.destroy_image_view(v, None);
                }
                self.vk.device.destroy_image_view(img.view, None);
                self.vk.device.destroy_image(img.image, None);
                self.vk.device.free_memory(img.memory, None);
            }
        }
    }
}

fn allocate(
    vk: &VkContext,
    format: vk::Format,
    extent: vk::Extent2D,
) -> Result<ReadbackImage, DstReadbackError> {
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
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { vk.device.create_image(&image_info, None)? };

    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let mt = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    });
    let mt = match mt {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(DstReadbackError::NoMemoryType);
        }
    };
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt)
        .push_next(&mut dedicated);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e.into());
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(e.into());
    }

    // R8 needs the swizzle (a = R) so the shader's `texture(...).a`
    // matches the X RENDER convention for alpha-only pictures.
    let components = if format == vk::Format::R8_UNORM {
        vk::ComponentMapping {
            r: vk::ComponentSwizzle::ZERO,
            g: vk::ComponentSwizzle::ZERO,
            b: vk::ComponentSwizzle::ZERO,
            a: vk::ComponentSwizzle::R,
        }
    } else {
        vk::ComponentMapping::default()
    };
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .components(components)
        .subresource_range(color_subresource_range());
    let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
        Ok(v) => v,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }
    };

    Ok(ReadbackImage {
        image,
        view,
        no_alpha_view: None,
        memory,
        extent,
        current_layout: vk::ImageLayout::UNDEFINED,
    })
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
