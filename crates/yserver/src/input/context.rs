//! libinput context wrapper.
//!
//! Owns an `input::Libinput` against udev seat0 with a `LibinputInterface`
//! that honours the flags libinput requests (per the libinput contract —
//! some devices are read-only, forcing O_RDWR breaks them). The context
//! exposes its fd for epoll integration and a `dispatch()` method that
//! pulls pending libinput events and translates the relevant subset to
//! [`InputEvent`].

use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
};

use input::{
    Event, Libinput, LibinputInterface,
    event::{
        EventTrait,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        pointer::{Axis, ButtonState, PointerEvent, PointerScrollEvent},
    },
};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};

use crate::input::event::InputEvent;

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let result = OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_ACCMODE == O_RDONLY) | (flags & O_ACCMODE == O_RDWR))
            .write((flags & O_ACCMODE == O_WRONLY) | (flags & O_ACCMODE == O_RDWR))
            .open(path);
        match result {
            Ok(file) => {
                log::info!("libinput: open_restricted ok: {}", path.display());
                Ok(file.into())
            }
            Err(err) => {
                log::warn!(
                    "libinput: open_restricted failed: {} -> {err}",
                    path.display()
                );
                Err(err.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

pub struct Context {
    libinput: Libinput,
}

/// Newtype wrapper around `Context` that implements `Send`.
/// SAFETY: The libinput thread is the sole owner. We need `Send` only
/// because the context crosses the spawn boundary into that thread.
pub struct SendContext(Context);
unsafe impl Send for SendContext {}

impl SendContext {
    pub fn new() -> io::Result<Self> {
        Context::new().map(Self)
    }

    pub fn fd(&self) -> RawFd {
        self.0.fd()
    }

    pub fn dispatch(&mut self) -> io::Result<Vec<InputEvent>> {
        self.0.dispatch()
    }
}

impl AsFd for SendContext {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Context {
    pub fn new() -> io::Result<Self> {
        log_input_devnodes();
        let mut libinput = Libinput::new_with_udev(Interface);
        libinput.udev_assign_seat("seat0").map_err(|()| {
            io::Error::other(
                "libinput: udev_assign_seat(\"seat0\") failed — is udev running and the \
                 seat reachable from this process?",
            )
        })?;
        Ok(Self { libinput })
    }

    pub fn fd(&self) -> RawFd {
        self.libinput.as_raw_fd()
    }

    pub fn dispatch(&mut self) -> io::Result<Vec<InputEvent>> {
        self.libinput.dispatch()?;
        let mut out = Vec::new();
        for event in &mut self.libinput {
            // Log device add/remove unconditionally so we can tell from
            // the server log whether libinput is seeing input hardware.
            // No devices ever logged → seat permission / udev issue.
            match &event {
                Event::Device(input::event::DeviceEvent::Added(d)) => {
                    log::info!("libinput: device added: {:?}", d.device().name());
                }
                Event::Device(input::event::DeviceEvent::Removed(d)) => {
                    log::info!("libinput: device removed: {:?}", d.device().name());
                }
                _ => {}
            }
            if let Some(translated) = translate(&event) {
                out.push(translated);
            }
        }
        Ok(out)
    }
}

impl AsFd for Context {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.libinput.as_fd()
    }
}

/// Best-effort `/dev/input/` enumeration logged at startup. Lets us
/// tell from the log whether the input nodes exist and whether our
/// process can stat / open them. udev rules from logind grant ACL on
/// `event*` to the active session; if we see `open: ok` here but
/// libinput's `open_restricted` fails, the seat is the wrong one.
fn log_input_devnodes() {
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(err) => {
            log::warn!("/dev/input: read_dir failed: {err}");
            return;
        }
    };
    let mut nodes: Vec<_> = dir.flatten().collect();
    nodes.sort_by_key(std::fs::DirEntry::file_name);
    for entry in nodes {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("event") {
            continue;
        }
        let path = entry.path();
        match OpenOptions::new().read(true).open(&path) {
            Ok(_f) => log::info!("/dev/input/{name_str}: open(O_RDONLY) ok"),
            Err(err) => log::warn!("/dev/input/{name_str}: open(O_RDONLY) failed: {err}"),
        }
    }
}

/// Finger/continuous scroll → `PointerScroll` v120 quantization.
/// Both event types expose only `scroll_value` (in cursor-pixel-
/// equivalent units, no v120 quantization). Convert at ~15 px per
/// logical wheel click (xwayland/Sway convention) → factor 8.
///
/// `has_axis(axis)` MUST be checked first: libinput emits a
/// `client bug: value requested for unset axis` error if
/// `scroll_value` is called for an axis the event doesn't carry.
fn finger_or_continuous_to_event<E>(ev: &E) -> Option<InputEvent>
where
    E: PointerScrollEvent,
{
    const PX_TO_V120: f64 = 8.0;
    let dx_v120 = if ev.has_axis(Axis::Horizontal) {
        (ev.scroll_value(Axis::Horizontal) * PX_TO_V120) as i32
    } else {
        0
    };
    let dy_v120 = if ev.has_axis(Axis::Vertical) {
        (ev.scroll_value(Axis::Vertical) * PX_TO_V120) as i32
    } else {
        0
    };
    if dx_v120 == 0 && dy_v120 == 0 {
        return None;
    }
    Some(InputEvent::PointerScroll { dx_v120, dy_v120 })
}

fn translate(event: &Event) -> Option<InputEvent> {
    match event {
        Event::Keyboard(KeyboardEvent::Key(key)) => {
            let keycode = key.key();
            Some(match key.key_state() {
                KeyState::Pressed => InputEvent::KeyPress { keycode },
                KeyState::Released => InputEvent::KeyRelease { keycode },
            })
        }
        Event::Pointer(PointerEvent::Motion(motion)) => Some(InputEvent::PointerMotion {
            dx: motion.dx(),
            dy: motion.dy(),
        }),
        Event::Pointer(PointerEvent::MotionAbsolute(motion)) => {
            // libinput's `absolute_x/y_transformed(W)` maps the device's full
            // axis range to `0..W`.  Pass a large W and divide to recover a
            // normalised 0..1 coordinate; the backend scales to scanout size.
            const SCALE: u32 = 1_000_000;
            Some(InputEvent::PointerMotionAbsolute {
                x_norm: motion.absolute_x_transformed(SCALE) / SCALE as f64,
                y_norm: motion.absolute_y_transformed(SCALE) / SCALE as f64,
            })
        }
        Event::Pointer(PointerEvent::Button(btn)) => Some(InputEvent::Button {
            code: btn.button(),
            pressed: btn.button_state() == ButtonState::Pressed,
        }),
        Event::Pointer(PointerEvent::ScrollWheel(ev)) => {
            // Wheel events come pre-quantized in v120 (120 = one click).
            // has_axis(axis) MUST be checked first: libinput emits a
            // `client bug: value requested for unset axis` error if
            // scroll_value_v120 is called for an axis the event doesn't
            // carry. A pure vertical wheel event has Horizontal unset.
            let dx_v120 = if ev.has_axis(Axis::Horizontal) {
                ev.scroll_value_v120(Axis::Horizontal) as i32
            } else {
                0
            };
            let dy_v120 = if ev.has_axis(Axis::Vertical) {
                ev.scroll_value_v120(Axis::Vertical) as i32
            } else {
                0
            };
            if dx_v120 == 0 && dy_v120 == 0 {
                return None;
            }
            Some(InputEvent::PointerScroll { dx_v120, dy_v120 })
        }
        Event::Pointer(PointerEvent::ScrollFinger(ev)) => finger_or_continuous_to_event(ev),
        Event::Pointer(PointerEvent::ScrollContinuous(ev)) => finger_or_continuous_to_event(ev),
        _ => None,
    }
}
