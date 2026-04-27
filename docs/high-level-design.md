# High-Level Design

## Purpose

This project is a modern X11 server written from scratch in Rust. The goal is
not to clone Xorg. The goal is to provide a practical X11 server that can run
real desktop environments, window managers, compositors, and applications while
dropping legacy baggage that is no longer needed on modern Linux systems.

The server should preserve the useful property of X11: it provides mechanisms
that applications and desktop environments can compose in flexible ways. At the
same time, the implementation should avoid inheriting Xorg's historical
architecture, legacy hardware model, unsafe parsing, and obsolete rendering
paths.

## Design Position

The project targets the modern X11 desktop contract, not full historical X11
coverage.

That means the server must be compatible with the behavior that current desktop
software actively depends on: windows, properties, atoms, selections, input,
focus, grabs, RandR-style monitor discovery, XKB, XInput2, compositing, damage,
presentation, and OpenGL/EGL/Vulkan client rendering paths.

It does not mean supporting every protocol feature that the X11 specification
requires, every extension Xorg implements, every old visual class, every old
font path, every hardware driver architecture, or every historical compatibility
quirk.

## Goals

- Run real X11 desktop environments and window managers without requiring Xorg.
- Support modern Linux graphics hardware through DRM/KMS, GBM, EGL, and Mesa.
- Provide nested backends for development and testing under an existing X11 or
  Wayland session.
- Use a compositor-native architecture with tear-free presentation by default.
- Support multiple physical monitors as outputs, not legacy X11 screens.
- Support per-output timing, refresh rates, VRR, color capabilities, and future
  HDR work.
- Implement the modern X11 desktop behavior expected by GTK, Qt, SDL, GLFW,
  Electron, and similar actively used software stacks.
- Provide a security model that isolates clients by default while preserving
  compatibility through controlled permissions and dummy responses where
  possible.
- Keep the implementation auditable by using generated or declarative protocol
  parsing where practical.
- Prefer explicit internal architecture over Xorg-compatible internal
  abstractions.

## Non-Goals

- Being a drop-in clone of Xorg internals.
- Supporting old server driver modules or a DDX-style hardware driver ABI.
- Supporting hardware that lacks Linux DRM/KMS and Mesa GBM support.
- Supporting multiple X11 screens.
- Supporting non-TrueColor legacy visuals as a first-class feature.
- Supporting indirect or remote GLX.
- Supporting endian-swapped clients unless a concrete modern use case appears.
- Implementing obsolete font functionality beyond what real clients still need.
- Guaranteeing compatibility with every historical X11 application.
- Preserving behavior that exists only because of Xorg implementation accidents,
  unless real desktop software depends on it.

## Compatibility Targets

Compatibility should be defined by real software, not by abstract protocol
coverage. Early targets should move from simple clients to full desktop
sessions.

Initial client targets:

- Basic X11 tools such as `xeyes`, `xclock`, `xterm`, and `xev`.
- Simple GTK2, GTK3, GTK4, Qt5, Qt6, SDL2, SDL3, GLFW, and Electron programs.
- OpenGL, EGL, and Vulkan clients using modern direct rendering paths.

Window manager and compositor targets:

- Openbox.
- i3.
- awesome.
- fluxbox.
- picom or an equivalent external compositor.

Desktop environment targets:

- Xfce.
- MATE.
- LXQt.

GNOME and KDE Plasma may be useful later validation targets, but they should
not define the first compatibility milestone. They carry more session, Wayland,
portal, and compositor-specific assumptions than lighter X11-first desktop
environments.

## Protocol Scope

The core protocol must cover enough behavior for modern clients and desktop
environments:

- Connection setup, authentication, byte order handling for native-endian
  clients, errors, events, and replies.
- Resource ID allocation, ownership, lookup, lifetime, and cleanup.
- Windows, pixmaps, cursors, colormaps where required, graphics contexts, and
  drawables.
- Atoms, properties, root window properties, and client messages.
- Window hierarchy, mapping, unmapping, reparenting, override-redirect windows,
  stacking, configure requests, and exposure behavior.
- Focus, grabs, pointer and keyboard events, crossing events, enter/leave
  semantics, and event masks.
- Selections, clipboard behavior, drag-and-drop support, and timestamp rules.
- Cursor handling and the subset of font behavior needed for cursors and legacy
  clients that still appear in practice.

Likely required extensions:

- BIG-REQUESTS.
- MIT-SHM.
- RANDR.
- XFIXES.
- DAMAGE.
- COMPOSITE.
- SYNC.
- PRESENT.
- SHAPE.
- RENDER.
- XKB.
- XInput2.
- GLX, limited to modern direct-rendering use cases.

Extension support should be versioned, tested against real clients, and allowed
to return conservative capability sets until the implementation is mature.

## Architecture

The server should be split into clear layers.

Protocol frontend:

- Accepts client connections.
- Parses requests.
- Serializes replies, events, and errors.
- Owns protocol dispatch.
- Avoids handwritten unsafe parsing where possible.

Object model:

- Owns clients, resources, windows, pixmaps, surfaces, atoms, properties,
  selections, input devices, outputs, and extension state.
- Enforces resource ownership and lifetime rules.
- Provides stable internal APIs that are not tied to Xorg internals.

Desktop semantics layer:

- Implements ICCCM and EWMH behavior.
- Maintains focus policy hooks, window manager interactions, root properties,
  client messages, selections, clipboard, DND, and session-facing conventions.
- Provides the behavior desktop environments expect even where it is not part
  of the wire protocol itself.

Compositor:

- Owns compositing by default.
- Tracks damage.
- Maintains per-window surfaces.
- Schedules presentation per output.
- Handles fullscreen optimization and possible direct scanout.
- Can expose compatibility behavior for external compositors.

Backend abstraction:

- Provides output discovery, buffer allocation, presentation, input events, and
  timing.
- Supports nested X11 first.
- Supports nested Wayland later.
- Supports standalone DRM/KMS once the protocol and compositor layers are
  stable enough.

Security and policy:

- Applies per-client permissions.
- Restricts global input observation, screen capture, clipboard reads, and
  cross-client inspection by default.
- Can return dummy data instead of protocol errors where that preserves
  compatibility.
- Provides an Xorg-compatible mode for users or desktop sessions that need
  unrestricted behavior.

## DRM/KMS Backend

The DRM/KMS backend should not be the first implementation target. KMS solves
mode setting and page flipping, not the full display server problem.

The standalone backend will need:

- `udev` for device discovery.
- `libinput` for input devices.
- DRM atomic modesetting for outputs, CRTCs, planes, and page flips.
- GBM for buffer allocation.
- EGL or Vulkan for rendering into buffers.
- Explicit sync support where available.
- Per-output presentation scheduling.
- Hotplug handling.
- Session management integration through logind or seatd.

The backend should support modern hardware first and avoid an Xorg-style server
driver ABI. If non-DRM hardware support is ever added, it should be an isolated
backend, not a core architectural concern.

## Graphics Model

The server should be compositor-first. Every top-level window should have a
surface that can be independently damaged, synchronized, composed, and
presented. The root window should not imply a single global framebuffer across
all monitors.

The compositor should support:

- Tear-free presentation by default.
- Per-output refresh rates.
- Mixed-refresh multi-monitor setups.
- VRR where the output and driver support it.
- Fullscreen bypass or direct scanout where safe.
- Low-latency presentation paths.
- Future color management and HDR extensions.

External compositors should be supported as compatibility clients where
possible, but the built-in compositor is the primary model.

## Security Model

The server should make cross-client access explicit without breaking ordinary
clients.

Default restrictions:

- A client should not freely inspect other clients' windows.
- A client should not record arbitrary windows or outputs without permission.
- A client should not receive global input without permission.
- Clipboard reads should be restricted to focused clients or explicit paste
  actions where practical.
- Global hotkeys with modifiers should be supported as a normal desktop feature.
- Global hotkeys without modifiers should require elevated permission.

Compatibility behavior:

- Unauthorized requests should return empty or dummy data where clients expect a
  successful reply.
- Errors should be reserved for cases where dummy data would make behavior worse.
- A compatibility mode should be available to behave more like Xorg.

## Development Strategy

Development should proceed in phases that reduce unknowns.

Phase 1: nested protocol core.

- Accept X11 clients.
- Complete setup and authentication.
- Implement resource IDs, atoms, properties, windows, basic events, and errors.
- Run simple clients such as `xeyes`, `xclock`, `xterm`, and `xev`.

Phase 2: desktop semantics.

- Implement ICCCM and EWMH behavior.
- Support selections, clipboard, focus, grabs, configure requests, reparenting,
  override-redirect windows, and root window properties.
- Run simple window managers.

Phase 3: toolkit compatibility.

- Add required extensions for GTK, Qt, SDL, GLFW, and Electron.
- Implement enough XKB, XInput2, RENDER, SHAPE, DAMAGE, COMPOSITE, SYNC, and
  PRESENT behavior for real applications.

Phase 4: accelerated clients.

- Support modern GLX/EGL/Vulkan direct-rendering paths.
- Support MIT-SHM and buffer sharing where required.
- Validate real GPU-accelerated clients.

Phase 5: full desktop sessions.

- Run Xfce, MATE, LXQt, and common standalone window manager sessions.
- Validate panels, launchers, notification daemons, clipboard managers,
  screen lockers, global shortcuts, and external compositors.

Phase 6: standalone DRM/KMS.

- Add libinput, udev, GBM, EGL/Vulkan, and atomic KMS support.
- Implement hotplug, multi-monitor presentation, session management, and
  fullscreen/direct-scanout paths.

Phase 7: security hardening.

- Add capability enforcement.
- Add permission prompts or launch-time permission configuration.
- Add dummy response behavior.
- Add compatibility mode.

## Engineering Principles

- Compatibility is measured with real software.
- Legacy features require a current user-facing use case.
- Backends are replaceable; protocol semantics are core.
- The compositor is part of the server design, not an optional afterthought.
- KMS is a backend concern, not the starting point.
- Security policy should be explicit, testable, and bypassable only by intended
  compatibility controls.
- The implementation should prefer small, testable subsystems over a monolithic
  Xorg-like architecture.

## Open Questions

- Should the initial implementation use existing protocol descriptions such as
  XCB XML, or define its own protocol schema optimized for Rust code generation?
- Should the compositor use OpenGL/EGL first, Vulkan first, or abstract both
  from the beginning?
- Should the project depend on Smithay components for DRM, GBM, input, and
  session management, or keep those integrations thinner and more direct?
- How should permission prompts be implemented before a full desktop shell
  exists?
- What should the first reference desktop session be: Openbox, i3, Xfce, or
  LXQt?
- Which behavior should be deliberately Xorg-compatible even if it is not
  desirable long term?
