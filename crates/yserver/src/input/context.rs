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
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        pointer::{ButtonState, PointerEvent},
    },
};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};

use crate::input::event::InputEvent;

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_ACCMODE == O_RDONLY) | (flags & O_ACCMODE == O_RDWR))
            .write((flags & O_ACCMODE == O_WRONLY) | (flags & O_ACCMODE == O_RDWR))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap_or(libc::EIO))
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

pub struct Context {
    libinput: Libinput,
}

impl Context {
    pub fn new() -> io::Result<Self> {
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
        Event::Pointer(PointerEvent::Button(btn)) => Some(InputEvent::Button {
            code: btn.button(),
            pressed: btn.button_state() == ButtonState::Pressed,
        }),
        _ => None,
    }
}
