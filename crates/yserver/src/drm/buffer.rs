use std::{io, mem, ptr::NonNull, sync::Arc};

use drm::{
    buffer::{Buffer as _, DrmFourcc},
    control::{Device as ControlDevice, dumbbuffer::DumbBuffer, framebuffer},
};

use crate::drm::Device;

pub struct Buffer {
    device: Arc<Device>,
    dumb: DumbBuffer,
    fb_id: framebuffer::Handle,
    ptr: NonNull<u8>,
    len: usize,
    width: u16,
    height: u16,
    stride: u32,
}

unsafe impl Send for Buffer {}

impl Buffer {
    pub fn new(device: Arc<Device>, width: u16, height: u16) -> io::Result<Self> {
        let mut dumb = device.create_dumb_buffer(
            (u32::from(width), u32::from(height)),
            DrmFourcc::Xrgb8888,
            32,
        )?;
        let fb_id = match device.add_framebuffer(&dumb, 24, 32) {
            Ok(fb) => fb,
            Err(err) => {
                let _ = device.destroy_dumb_buffer(dumb);
                return Err(err);
            }
        };
        let mapped = device.map_dumb_buffer(&mut dumb).map(|m| {
            let len = m.len();
            let ptr = NonNull::new(m.as_ptr() as *mut u8).expect("non-null mmap");
            mem::forget(m);
            (ptr, len)
        });
        let (ptr, len) = match mapped {
            Ok(pair) => pair,
            Err(err) => {
                let _ = device.destroy_framebuffer(fb_id);
                let _ = device.destroy_dumb_buffer(dumb);
                return Err(err);
            }
        };

        let stride = dumb.pitch();
        Ok(Self {
            device,
            dumb,
            fb_id,
            ptr,
            len,
            width,
            height,
            stride,
        })
    }

    pub fn fb_id(&self) -> framebuffer::Handle {
        self.fb_id
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let words = self.len / 4;
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().cast::<u32>(), words) }
    }

    pub fn fill(&mut self, pixel: u32) {
        for word in self.pixels_mut() {
            *word = pixel;
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let Err(err) = self.device.destroy_framebuffer(self.fb_id) {
            log::warn!(
                "destroy_framebuffer 0x{:x} failed: {err}",
                u32::from(self.fb_id)
            );
        }
        unsafe {
            if libc::munmap(self.ptr.as_ptr().cast(), self.len) != 0 {
                log::warn!("munmap dumb buffer failed: {}", io::Error::last_os_error());
            }
        }
        if let Err(err) = self.device.destroy_dumb_buffer(self.dumb) {
            log::warn!("destroy_dumb_buffer failed: {err}");
        }
    }
}
