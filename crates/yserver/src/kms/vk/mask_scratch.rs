//! Backend-wide R8 mask scratch image (sub-phase 4.1.4.7).
//!
//! Some RENDER ops (`Trapezoids`, `Triangles`, glyph-shape composites
//! that aren't through the atlas) need an A8 coverage mask that
//! doesn't correspond to any X drawable. CPU rasterise the mask
//! into a per-batch `BatchUploadArena` chunk, then call
//! [`MaskScratch::record_upload_r8`] to record the
//! barrier-copy-barrier sequence that uploads it into a transient
//! `R8_UNORM` `VkImage`. The image view is lifetime-stable as long
//! as the caller doesn't request a bigger mask; resize via
//! [`MaskScratch::ensure_image_size_returning_old`] allocates a new
//! image, returns the old one wrapped as a `BatchResource` for
//! defer-release, and installs the new fields on the scratch.
//!
//! 5-T5: defer-release replaces the pre-Phase-5 pre-flush gate —
//! the old image survives any in-flight CB because the scheduler
//! holds it until the open `PaintBatch` retires (or releases it
//! synchronously if no in-flight batch exists).
//!
//! Single shared scratch — the renderer schedules render-traps
//! into PaintBatches that submit before the next protocol cycle,
//! so the scratch only has pending uses inside the current
//! recording batch.

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;
use crate::kms::scheduler::paint_batch::BatchResource;

#[derive(Debug, thiserror::Error)]
pub enum MaskScratchError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no memory type matches mask scratch requirements")]
    NoMemoryType,
}

impl From<vk::Result> for MaskScratchError {
    fn from(r: vk::Result) -> Self {
        MaskScratchError::Vk(r)
    }
}

/// 5-T5: wraps the just-replaced scratch image (image + view +
/// memory) so the scheduler can release it after the in-flight batch
/// retires. The new image has already been installed on `MaskScratch`
/// by the time this is constructed.
#[derive(Debug)]
struct RetiredMaskScratchImage {
    image: vk::Image,
    view: vk::ImageView,
    image_memory: vk::DeviceMemory,
}

impl BatchResource for RetiredMaskScratchImage {
    fn release(self: Box<Self>, vk: &VkContext) {
        unsafe {
            vk.device.destroy_image_view(self.view, None);
            vk.device.destroy_image(self.image, None);
            vk.device.free_memory(self.image_memory, None);
        }
    }
}

pub struct MaskScratch {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    image_memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    current_layout: vk::ImageLayout,
}

impl MaskScratch {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, MaskScratchError> {
        // Start at 256×256; grow on demand. Most RENDER traps/triangle
        // workloads (cairo glyph shapes) fit; rendercheck pushes
        // ~1024×1024 in worst-case test rects.
        let extent = vk::Extent2D {
            width: 256,
            height: 256,
        };
        let (image, view, image_memory) = allocate_image(&vk, extent)?;
        Ok(Self {
            vk,
            image,
            view,
            image_memory,
            extent,
            current_layout: vk::ImageLayout::UNDEFINED,
        })
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub fn image_view(&self) -> vk::ImageView {
        self.view
    }

    /// 5-T5: ensure the scratch image is at least `(width, height)`
    /// pixels, reallocating if smaller. Returns `Ok(None)` if no grow
    /// was needed (scratch unchanged). On grow, allocates the new
    /// resource first (so a failure leaves the scratch untouched),
    /// then installs the new fields and returns the old image
    /// wrapped as a `BatchResource` for the caller to defer-release
    /// through the scheduler.
    ///
    /// Allocation-first ordering: if `allocate_image` fails the
    /// scratch is untouched and the caller sees `Err`. Past the
    /// successful `allocate_image` the function MUST NOT fail.
    pub fn ensure_image_size_returning_old(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Option<Box<dyn BatchResource>>, MaskScratchError> {
        if width <= self.extent.width && height <= self.extent.height {
            return Ok(None);
        }
        let new_extent = vk::Extent2D {
            width: self.extent.width.max(width).next_power_of_two().max(256),
            height: self.extent.height.max(height).next_power_of_two().max(256),
        };
        let (image, view, image_memory) = allocate_image(&self.vk, new_extent)?;
        // Allocation succeeded — past here the function MUST NOT fail.
        let old_image = std::mem::replace(&mut self.image, image);
        let old_view = std::mem::replace(&mut self.view, view);
        let old_memory = std::mem::replace(&mut self.image_memory, image_memory);
        self.extent = new_extent;
        self.current_layout = vk::ImageLayout::UNDEFINED;
        Ok(Some(Box::new(RetiredMaskScratchImage {
            image: old_image,
            view: old_view,
            image_memory: old_memory,
        })))
    }

    /// Record the barrier-copy-barrier sequence that uploads
    /// `width × height` R8 pixels from `src_buffer + src_offset`
    /// to the scratch image's top-left `(0, 0)` rect, into the
    /// supplied CB. After this returns the image's `current_layout`
    /// reflects `SHADER_READ_ONLY_OPTIMAL` (the CB's terminal
    /// transition); on CB execute the image lands in that layout.
    ///
    /// Caller is responsible for:
    ///   1. Calling `ensure_image_size_returning_old(width, height)?`
    ///      BEFORE this method, and routing any returned old image
    ///      through `RenderScheduler::defer_resource_release` so the
    ///      old `vk::Image`/view/memory outlives any in-flight CB.
    ///   2. Allocating staging via `BatchUploadArena::alloc(width *
    ///      height, 4)` and copying the row-major coverage bytes
    ///      into the returned `mapped_ptr`.
    ///   3. Passing the resulting `buffer` + `offset` here.
    ///
    /// `width` / `height` must be ≤ `self.extent` (no grow inside
    /// this method); zero-sized rects no-op.
    pub fn record_upload_r8(
        &mut self,
        vk: &VkContext,
        cb: vk::CommandBuffer,
        src_buffer: vk::Buffer,
        src_offset: u64,
        width: u32,
        height: u32,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        debug_assert!(
            width <= self.extent.width && height <= self.extent.height,
            "MaskScratch::record_upload_r8: caller must ensure_image_size_returning_old first",
        );
        let device = &vk.device;
        let old_layout = self.current_layout;
        let to_dst = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        let region = vk::BufferImageCopy::default()
            .buffer_offset(src_offset)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D::default())
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        let regions = [region];
        unsafe {
            device.cmd_copy_buffer_to_image(
                cb,
                src_buffer,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
        }

        let to_read = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }
}

impl Drop for MaskScratch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.image_memory, None);
        }
    }
}

fn allocate_image(
    vk: &VkContext,
    extent: vk::Extent2D,
) -> Result<(vk::Image, vk::ImageView, vk::DeviceMemory), MaskScratchError> {
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8_UNORM)
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
            return Err(MaskScratchError::NoMemoryType);
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
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8_UNORM)
        // Swizzle so .a samples R — same convention the Composite
        // shader already uses for R8 mask drawables.
        .components(vk::ComponentMapping {
            r: vk::ComponentSwizzle::ZERO,
            g: vk::ComponentSwizzle::ZERO,
            b: vk::ComponentSwizzle::ZERO,
            a: vk::ComponentSwizzle::R,
        })
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
    Ok((image, view, memory))
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
