# yserver

A modern X11 server written from scratch in Rust.

The goal is not to clone Xorg. It is to provide a practical X11 server that
runs real desktop environments, window managers, and applications on modern
Linux while dropping legacy baggage (multiple screens, non-TrueColor visuals,
indirect GLX, the DDX driver ABI, endian-swapped clients, and so on).

See [`docs/high-level-design.md`](docs/high-level-design.md) for the full
design, scope, and phased plan.

## Status

Early Phase 1: a nested backend (`ynest`) accepts X11 client connections over
a Unix socket and forwards drawing into a host X11 window. Simple clients
(`xeyes`, `xclock`, `xterm`) come up. Many requests are still stubbed.

The standalone DRM/KMS binary (`yserver`) is a placeholder.

See [`docs/status.md`](docs/status.md) for per-phase progress and the
current Phase 1 punch list.

## Layout

- `crates/yserver-protocol` — wire-format types, request/reply parsing.
- `crates/yserver-core` — server core: client dispatch, resources, nested
  backend (`nested.rs`), host X11 forwarding (`host_x11.rs`).
- `crates/yserver` — the `ynest` and `yserver` binaries.

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build
```

For a release build:

```sh
cargo build --release
```

## Running the nested server

`ynest` listens on a Unix socket at `/tmp/.X11-unix/X<display>` and renders
into a window on the host X server pointed to by `$DISPLAY`. You need an
existing X11 (or XWayland) session to host it.

Start it on display `:99` (the default):

```sh
cargo run --bin ynest
```

Or pick a different display number:

```sh
cargo run --bin ynest -- 42
```

Then point clients at it from another terminal, using the same display
number you started `ynest` on:

```sh
DISPLAY=:42 xeyes
DISPLAY=:42 xclock
DISPLAY=:42 xterm
```

A host window titled by `ynest` appears; nested client output is drawn into
it. Keyboard input over that window is forwarded to the focused nested
client.

## Development

Before committing:

```sh
cargo fmt
cargo clippy -- -W clippy::pedantic
cargo test
```
