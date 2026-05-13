//! Backend-wide R8 mask scratch image (sub-phase 4.1.4.7).
//!
//! Some RENDER ops (`Trapezoids`, `Triangles`, glyph-shape composites
//! that aren't through the atlas) need an A8 coverage mask that
//! doesn't correspond to any X drawable. CPU rasterise the mask into
//! the host-mapped staging buffer here, then `flush_to_image()`
//! uploads it to a transient `R8_UNORM` `VkImage` and transitions
//! the layout to `SHADER_READ_ONLY_OPTIMAL`. The image view is
//! lifetime-stable as long as the caller doesn't request a bigger
//! mask; resize re-allocates and invalidates the old view.
//!
//! Single shared scratch — RENDER ops are eager-submitted (per
//! `feedback_phase4_1_4_decisions.md` §2), so the scratch never has
//! pending uses by the time the next Composite call records its CB.

use std::{ptr::NonNull, sync::Arc};

use ash::vk;

use super::{device::VkContext, ops::run_one_shot_op};

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

pub struct MaskScratch {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    image_memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    current_layout: vk::ImageLayout,
    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_mapped: NonNull<u8>,
    staging_size: u64,
}

unsafe impl Send for MaskScratch {}
unsafe impl Sync for MaskScratch {}

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
        let initial_staging = u64::from(extent.width) * u64::from(extent.height);
        let (staging_buffer, staging_memory, staging_mapped) =
            match allocate_staging(&vk, initial_staging) {
                Ok(t) => t,
                Err(e) => {
                    unsafe {
                        vk.device.destroy_image_view(view, None);
                        vk.device.destroy_image(image, None);
                        vk.device.free_memory(image_memory, None);
                    }
                    return Err(e);
                }
            };
        Ok(Self {
            vk,
            image,
            view,
            image_memory,
            extent,
            current_layout: vk::ImageLayout::UNDEFINED,
            staging_buffer,
            staging_memory,
            staging_mapped,
            staging_size: initial_staging,
        })
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub fn image_view(&self) -> vk::ImageView {
        self.view
    }

    /// True if a later `ensure_image_size(width, height)` call would
    /// reallocate the per-format scratch image. Callers in batched
    /// paint paths use this BEFORE entering `record_paint_batch_op`
    /// so they can flush any in-flight batch — `ensure_image_size`
    /// destroys the old image after `queue_wait_idle`, which does
    /// NOT wait for un-submitted commands. Without a pre-flush, an
    /// open batch CB embedding the old scratch image would dangle.
    /// Mirrors `DstReadback::needs_grow` (3F-1) and
    /// `CopyScratch::needs_grow` (3D).
    pub fn needs_image_grow(&self, width: u32, height: u32) -> bool {
        width > self.extent.width || height > self.extent.height
    }

    /// Ensure the scratch image is at least `(width, height)` pixels,
    /// reallocating if smaller. After this returns the image is in
    /// `UNDEFINED` layout (treated as new) when reallocation happens.
    fn ensure_image_size(&mut self, width: u32, height: u32) -> Result<(), MaskScratchError> {
        if width <= self.extent.width && height <= self.extent.height {
            return Ok(());
        }
        let new_extent = vk::Extent2D {
            width: self.extent.width.max(width).next_power_of_two().max(256),
            height: self.extent.height.max(height).next_power_of_two().max(256),
        };
        let (image, view, image_memory) = allocate_image(&self.vk, new_extent)?;
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.image_memory, None);
        }
        self.image = image;
        self.view = view;
        self.image_memory = image_memory;
        self.extent = new_extent;
        self.current_layout = vk::ImageLayout::UNDEFINED;
        Ok(())
    }

    fn ensure_staging(&mut self, needed: u64) -> Result<(), MaskScratchError> {
        if needed <= self.staging_size {
            return Ok(());
        }
        let mut new_size = self.staging_size.max(64 * 1024);
        while new_size < needed {
            new_size = new_size.checked_mul(2).unwrap_or(needed);
        }
        let (buffer, memory, mapped) = allocate_staging(&self.vk, new_size)?;
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.staging_memory);
            self.vk.device.destroy_buffer(self.staging_buffer, None);
            self.vk.device.free_memory(self.staging_memory, None);
        }
        self.staging_buffer = buffer;
        self.staging_memory = memory;
        self.staging_mapped = mapped;
        self.staging_size = new_size;
        Ok(())
    }

    /// Copy `bytes` (row-major, no padding, `width * height` long)
    /// into staging and upload to the scratch image's top-left
    /// `(0, 0)` rect of size `width × height`. Image transitions to
    /// `SHADER_READ_ONLY_OPTIMAL` on return.
    pub fn upload_r8(
        &mut self,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<(), MaskScratchError> {
        debug_assert_eq!(bytes.len(), (width * height) as usize);
        if width == 0 || height == 0 {
            return Ok(());
        }
        self.ensure_image_size(width, height)?;
        let needed = u64::from(width) * u64::from(height);
        self.ensure_staging(needed)?;

        // SAFETY: staging is mapped HOST_VISIBLE | HOST_COHERENT and
        // sized ≥ `needed`. `bytes.len() == needed`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.staging_mapped.as_ptr(),
                bytes.len(),
            );
        }

        let buffer = self.staging_buffer;
        let image = self.image;
        let old_layout = self.current_layout;

        run_one_shot_op(&self.vk, pool, |vk, cb| {
            let device = &vk.device;
            let to_dst = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(image)
                .subresource_range(color_subresource_range())];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
            unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
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
                    buffer,
                    image,
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
                .image(image)
                .subresource_range(color_subresource_range())];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
            unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
            Ok(())
        })?;
        self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        Ok(())
    }
}

impl Drop for MaskScratch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.staging_memory);
            self.vk.device.destroy_buffer(self.staging_buffer, None);
            self.vk.device.free_memory(self.staging_memory, None);
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

fn allocate_staging(
    vk: &VkContext,
    size: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory, NonNull<u8>), MaskScratchError> {
    let buf_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };
    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let mt = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(want)
    });
    let mt = match mt {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(MaskScratchError::NoMemoryType);
        }
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(e.into());
        }
    };
    if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_buffer(buffer, None);
        }
        return Err(e.into());
    }
    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(e.into());
        }
    };
    let mapped = NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");
    Ok((buffer, memory, mapped))
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
