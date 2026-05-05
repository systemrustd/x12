use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::io::{AsFd, BorrowedFd},
};

use drm::{ClientCapability, Device as DrmDevice};

pub struct Device {
    file: File,
    path: String,
}

impl AsFd for Device {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl DrmDevice for Device {}
impl drm::control::Device for Device {}

impl Device {
    #[cfg(test)]
    pub(crate) fn for_tests() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        Ok(Self {
            file,
            path: "/dev/null".to_string(),
        })
    }

    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|err| open_error(path, &err))?;
        let device = Self {
            file,
            path: path.to_string(),
        };
        device.acquire_master_lock().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to acquire DRM master on {path}: {err}"),
            )
        })?;
        device.enable_atomic_capabilities()?;
        Ok(device)
    }

    fn enable_atomic_capabilities(&self) -> io::Result<()> {
        for cap in [ClientCapability::UniversalPlanes, ClientCapability::Atomic] {
            self.set_client_capability(cap, true).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "DRM driver does not support {cap:?} — virtio-gpu in modern kernels \
                         supports both atomic and universal planes; check kernel and \
                         qemu-desktop versions: {err}"
                    ),
                )
            })?;
        }
        Ok(())
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Err(err) = self.release_master_lock() {
            log::warn!(
                "failed to release DRM master on {}: {err} (file close will still drop the fd)",
                self.path
            );
        }
    }
}

fn open_error(path: &str, err: &io::Error) -> io::Error {
    use io::ErrorKind;
    let msg = match err.kind() {
        ErrorKind::NotFound => format!(
            "DRM device {path} not found. In vng: pass --graphics or \
             --qemu-opts=\"-device virtio-gpu-pci\". On bare metal: check `ls /dev/dri/` — \
             the GPU may be at card1 instead. Override with \
             `YSERVER_DRM_DEVICE=/dev/dri/cardN`."
        ),
        ErrorKind::PermissionDenied => format!(
            "opening {path} requires root — vng runs as root by default; on host use sudo \
             (but B is vng-only by design)"
        ),
        _ if err.raw_os_error() == Some(libc::EBUSY) => format!(
            "another DRM master holds {path} — B is vng-only; do not run yserver on a host \
             with an active graphical session"
        ),
        _ => format!("failed to open {path}: {err}"),
    };
    io::Error::new(err.kind(), msg)
}
