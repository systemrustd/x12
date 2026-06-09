//! GLX texture-from-pixmap: exportable-image allocation + dma-buf export.
//!
//! Vulkan-gated — run with:
//!   cargo test --test glx_tfp_export -- --ignored

#![cfg(target_os = "linux")]

use std::sync::Arc;

use ash::vk;
use yserver::kms::{
    v2::KmsBackendV2,
    vk::{device::VkContext, dri3::DmabufExport},
};
use yserver_core::backend::Backend;

#[test]
#[ignore = "requires a Vulkan device"]
fn allocate_exportable_yields_valid_dmabuf_fd() {
    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run on HW");

    let img = yserver::kms::vk::target::allocate_exportable(
        &vk,
        /* width */ 64,
        /* height */ 32,
        yserver::kms::vk::target::EXPORT_FORMAT_BGRA8,
    )
    .expect("allocate exportable image");

    // Stride/size from vkGetImageSubresourceLayout must be sane.
    assert!(
        img.stride >= 64 * 4,
        "stride {} too small for 64px BGRA8 row",
        img.stride
    );
    assert!(
        img.size as usize >= img.stride as usize * 32,
        "size {} too small for {}*32",
        img.size,
        img.stride
    );

    // Export must succeed and carry sane stride/size.
    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).expect("export_backing failed");
    assert!(
        export.stride >= 64 * 4,
        "export stride {} too small",
        export.stride
    );
    assert!(
        export.size >= export.stride * 32,
        "export size {} too small for stride {} * 32 rows",
        export.size,
        export.stride
    );
}

// ───────────────────────────────────────────────────────────────────
// Task 1.2: pixmap promotion liveness test + harness
// ───────────────────────────────────────────────────────────────────

/// Minimal engine/store/platform harness built atop the production
/// `KmsBackendV2` (the only construction path that wires a real
/// engine + store + platform with a live VkContext). All drawable
/// manipulation goes through the public `Backend` trait + the
/// `*_for_tests` shims, so this harness never reaches into the
/// crate-private engine internals directly.
struct PromoteHarness {
    backend: KmsBackendV2,
    vk: Arc<VkContext>,
}

impl PromoteHarness {
    /// Create a depth-`depth` pixmap of `w`×`h`. Returns the host xid.
    fn create_pixmap(&mut self, w: u16, h: u16, depth: u8) -> u32 {
        let handle = self
            .backend
            .create_pixmap(None, depth, w, h)
            .expect("create_pixmap");
        handle.as_raw()
    }

    /// Fill the whole pixmap with a solid 0xRRGGBB colour (the X11
    /// foreground convention used by `poly_fill_rectangle`: an ARGB
    /// pixel, with alpha forced opaque).
    fn fill_solid(&mut self, xid: u32, rgb: u32, w: u16, h: u16) {
        let mut rect = Vec::new();
        rect.extend_from_slice(&i16::to_le_bytes(0)); // x
        rect.extend_from_slice(&i16::to_le_bytes(0)); // y
        rect.extend_from_slice(&u16::to_le_bytes(w)); // w
        rect.extend_from_slice(&u16::to_le_bytes(h)); // h
        self.backend
            .poly_fill_rectangle(None, xid, 0xFF00_0000 | rgb, &rect)
            .expect("poly_fill_rectangle");
    }

    /// Close any open frame batch and wait on every in-flight fence so
    /// the GPU has actually landed all queued paints.
    fn flush_and_wait(&mut self) {
        self.backend
            .engine_close_open_frame_for_timeout_for_tests()
            .expect("close open frame");
        self.backend.engine_drain_all_for_tests();
    }

    /// Promote the pixmap onto exportable storage and export the
    /// resulting dma-buf.
    fn promote_and_export(&mut self, xid: u32) -> std::io::Result<DmabufExport> {
        self.backend.promote_and_export_pixmap_for_tests(xid)
    }
}

/// Build the harness, or `None` if no Vulkan device is present.
fn test_engine_harness() -> Option<PromoteHarness> {
    let backend = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return None;
        }
    };
    let vk = backend.test_vk_arc().expect("vk arc present");
    Some(PromoteHarness { backend, vk })
}

/// Read the first BGRA pixel (as 0xRRGGBB, alpha dropped) out of the
/// exported dma-buf. Re-imports the fd through the production
/// `DrawableImage::from_dmabuf` path, then `vkCmdCopyImageToBuffer`s
/// into a HOST_VISIBLE staging buffer — DEVICE_LOCAL exported memory
/// is not CPU-mappable on a dGPU, so a raw mmap of the fd is not used.
fn read_dmabuf_pixel0(vk: &Arc<VkContext>, exported: &DmabufExport, w: u32, h: u32) -> u32 {
    use std::os::fd::AsFd;
    use yserver::kms::vk::target::{DrawableImage, EXPORT_FORMAT_BGRA8};

    // Re-import: dup the fd so the original `exported.fd` stays owned
    // by the caller.
    let dup_fd = exported.fd.as_fd().try_clone_to_owned().expect("dup fd");
    let img = DrawableImage::from_dmabuf(
        Arc::clone(vk),
        dup_fd,
        w,
        h,
        EXPORT_FORMAT_BGRA8,
        0, // DRM_FORMAT_MOD_LINEAR
        &[0],
        &[exported.stride],
    )
    .expect("from_dmabuf re-import");

    // HOST_VISIBLE staging buffer for the readback.
    let buf_size = u64::from(w * h * 4);
    let buf_info = vk::BufferCreateInfo::default()
        .size(buf_size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { vk.device.create_buffer(&buf_info, None) }.expect("create_buffer");
    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let mt = (0..mem_props.memory_type_count)
        .find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize].property_flags.contains(
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
        })
        .expect("host-visible memory type");
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = unsafe { vk.device.allocate_memory(&alloc, None) }.expect("allocate_memory");
    unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) }.expect("bind_buffer_memory");

    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::TRANSIENT);
    let pool = unsafe { vk.device.create_command_pool(&pool_info, None) }.expect("create pool");

    yserver::kms::vk::ops::run_one_shot_op(vk, pool, |vk, cb| {
        // Imported dma-buf comes in UNDEFINED → TRANSFER_SRC.
        let to_src = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(img.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_src);
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };

        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })];
        unsafe {
            vk.device.cmd_copy_image_to_buffer(
                cb,
                img.vk_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &region,
            );
        }
        Ok(())
    })
    .expect("readback one-shot op");

    let pixel = unsafe {
        let ptr = vk
            .device
            .map_memory(memory, 0, buf_size, vk::MemoryMapFlags::empty())
            .expect("map_memory") as *const u8;
        let b = u32::from(*ptr);
        let g = u32::from(*ptr.add(1));
        let r = u32::from(*ptr.add(2));
        vk.device.unmap_memory(memory);
        (r << 16) | (g << 8) | b
    };

    unsafe {
        vk.device.destroy_command_pool(pool, None);
        vk.device.destroy_buffer(buffer, None);
        vk.device.free_memory(memory, None);
    }
    pixel
}

#[test]
#[ignore = "requires a Vulkan device"]
fn promotion_preserves_content_and_is_live() {
    let Some(mut h) = test_engine_harness() else {
        return;
    };

    // Create a normal (non-exportable) server-owned pixmap, fill red.
    let pix = h.create_pixmap(64, 32, 24);
    h.fill_solid(pix, 0xFF_00_00, 64, 32); // red
    h.flush_and_wait();

    // Promote it. Old image must be retired, content preserved.
    let exported = h.promote_and_export(pix).expect("promote + export");
    let vk = Arc::clone(&h.vk);
    let pixel = read_dmabuf_pixel0(&vk, &exported, 64, 32);
    assert_eq!(pixel, 0xFF_00_00, "promoted image lost original content");

    // Liveness: a fill AFTER promotion lands in the exported backing.
    h.fill_solid(pix, 0x00_FF_00, 64, 32); // green
    h.flush_and_wait();
    let pixel2 = read_dmabuf_pixel0(&vk, &exported, 64, 32);
    assert_eq!(
        pixel2, 0x00_FF_00,
        "post-promotion write not visible in dmabuf — not live"
    );
}
