//! yserver-local input event enum.
//!
//! Deliberately minimal: keycodes, pointer deltas, button + state.
//! No keysym translation — that's xkbcommon's job and lives in C.

#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    KeyPress { keycode: u32 },
    KeyRelease { keycode: u32 },
    /// Relative pointer motion (mouse).
    PointerMotion { dx: f64, dy: f64 },
    /// Absolute pointer motion (tablet).  Coordinates are in 0..1 over the
    /// device's logical surface; the backend scales to scanout dimensions.
    PointerMotionAbsolute { x_norm: f64, y_norm: f64 },
    Button { code: u32, pressed: bool },
}
