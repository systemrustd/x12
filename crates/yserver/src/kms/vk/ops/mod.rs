//! Vulkan-direct drawing-op recorders (sub-phase 4.1.4).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! "Data flow (drawing pipeline)".
//!
//! Each module records a single op family into a caller-supplied
//! `vk::CommandBuffer`. Ops bracket their own
//! `vkCmdBeginRendering` / `vkCmdEndRendering` and the matching
//! `SHADER_READ_ONLY_OPTIMAL ↔ COLOR_ATTACHMENT_OPTIMAL` layout
//! transitions, leaving the mirror in `SHADER_READ_ONLY_OPTIMAL`
//! when they return so the next composite-pass read sees a layout
//! it can sample.
//!
//! 4.1.4.1: solid-fill family ([`fill`]).
//! 4.1.4.2-4.1.4.9: each lands its own module here.

pub mod copy;
pub mod fill;
pub mod image;
pub mod render;
pub mod text;
pub mod traps;

use std::{ptr::NonNull, sync::Arc};

use ash::vk;

use super::device::VkContext;

/// RAII wrapper around the per-backend drawing-op command pool —
/// separate pool from `MirrorUploader`'s transfer pool so graphics
/// and transfer workloads don't share CB lifetimes. One-shot
/// `with_ops_cb` allocates a primary CB from this pool, records,
/// submits + waits idle, and returns the CB to the pool via
/// `vkResetCommandBuffer` (the pool was created with the
/// `RESET_COMMAND_BUFFER` flag).
pub struct OpsCommandPool {
    vk: Arc<VkContext>,
    pool: vk::CommandPool,
}

impl OpsCommandPool {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, vk::Result> {
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(vk.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };
        Ok(Self { vk, pool })
    }

    pub fn handle(&self) -> vk::CommandPool {
        self.pool
    }
}

impl Drop for OpsCommandPool {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_command_pool(self.pool, None);
        }
    }
}

/// Allocate a one-shot primary CB from `pool`, run `record`
/// against it, submit + `vkQueueWaitIdle`, free the CB.
///
/// Free function (not a method on `KmsBackend`) so the caller can
/// hold a `&mut DrawableImage` borrow into the closure without
/// fighting the borrow checker over `&self` vs `&mut self.windows`.
/// Per-op submit + wait_idle is the simple cadence used through
/// the 4.1.4 family port; batch CBs land with 4.1.4.6 RENDER
/// `Composite` where op rate spikes.
pub fn run_one_shot_op<F>(
    vk: &VkContext,
    pool: vk::CommandPool,
    record: F,
) -> Result<(), vk::Result>
where
    F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
{
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { vk.device.allocate_command_buffers(&alloc_info)?[0] };

    let result = (|| -> Result<(), vk::Result> {
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { vk.device.begin_command_buffer(cb, &begin)? };
        record(vk, cb)?;
        unsafe { vk.device.end_command_buffer(cb)? };

        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
        unsafe {
            vk.device
                .queue_submit2(vk.graphics_queue, &submit, vk::Fence::null())?;
            vk.device.queue_wait_idle(vk.graphics_queue)?;
        }
        Ok(())
    })();

    // Free the CB regardless of recording outcome; the pool is
    // RESET_COMMAND_BUFFER, individual frees are cheap.
    unsafe { vk.device.free_command_buffers(pool, &[cb]) };
    result
}

/// Host-mapped, growable staging buffer used by image-transfer ops
/// (`PutImage`, `GetImage`, `MitShmPutImage`, `MitShmGetImage`,
/// `MitShmCreatePixmap`). One per backend, reused across ops.
///
/// `TRANSFER_SRC | TRANSFER_DST` so the same buffer serves uploads
/// (`vkCmdCopyBufferToImage`) and readbacks (`vkCmdCopyImageToBuffer`).
/// `HOST_VISIBLE | HOST_COHERENT` so the CPU can memcpy in (PutImage)
/// and read back (GetImage) without explicit flush/invalidate.
pub struct OpsStaging {
    vk: Arc<VkContext>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: NonNull<u8>,
    size: u64,
}

unsafe impl Send for OpsStaging {}
unsafe impl Sync for OpsStaging {}

impl OpsStaging {
    pub fn new(vk: Arc<VkContext>, initial_size: u64) -> Result<Self, vk::Result> {
        let (buffer, memory, mapped) = allocate_ops_staging(&vk, initial_size)?;
        Ok(Self {
            vk,
            buffer,
            memory,
            mapped,
            size: initial_size,
        })
    }

    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    pub fn mapped_ptr(&self) -> *mut u8 {
        self.mapped.as_ptr()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Grow if needed so `at_least` bytes fit. Idle-waits the graphics
    /// queue before tearing the old buffer down — eager-submit-per-op
    /// means there's no in-flight work referencing it, but cheap to be
    /// safe.
    pub fn ensure(&mut self, at_least: u64) -> Result<(), vk::Result> {
        if at_least <= self.size {
            return Ok(());
        }
        let mut new_size = self.size.max(1024 * 1024);
        while new_size < at_least {
            new_size = new_size.checked_mul(2).unwrap_or(at_least);
        }
        let (buffer, memory, mapped) = allocate_ops_staging(&self.vk, new_size)?;
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
        self.buffer = buffer;
        self.memory = memory;
        self.mapped = mapped;
        self.size = new_size;
        Ok(())
    }
}

impl Drop for OpsStaging {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

fn allocate_ops_staging(
    vk: &VkContext,
    size: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory, NonNull<u8>), vk::Result> {
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
    let memory_type_index = (0..mem_props.memory_type_count)
        .find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        })
        .ok_or(vk::Result::ERROR_FEATURE_NOT_PRESENT);
    let memory_type_index = match memory_type_index {
        Ok(i) => i,
        Err(e) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(e);
        }
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
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
            return Err(e);
        }
    };
    let mapped = NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");
    Ok((buffer, memory, mapped))
}
