# Phase 3.5 — High-Leverage Extension Completion Design

## Goal

Land four extension-completion items that unblock real WM/desktop
workloads on `ynest`:

1. **MIT-SHM** (new extension) — wraster-style icon composition (wmaker
   appicons, Qt/GTK fast-path image upload), confirmed via x11trace as
   the gating extension behind the wmaker empty-appicon bug.
2. **DAMAGE auto-accumulation** — automatic damage tracking on every
   drawing op, so compositors / screen recorders / accessibility tools
   that subscribe via `DamageCreate` actually receive
   `DamageNotify` events.
3. **COMPOSITE `NameWindowPixmap`** — currently returns `BadMatch`;
   real compositors (picom, mutter, kwin) need it to grab a snapshot of
   a redirected window's content.
4. **RENDER `ChangePicture` XID attributes** — finish the `CPClipMask`
   and `CPAlphaMap` paths (when set to a real pixmap/picture rather
   than `None`); modern themes hit this for Xft text clipping.

Phase 3.5 is a feature phase, not a polish phase. Three of the four
items add real protocol surface (one new extension, two new code paths
inside existing extensions). One is a behavioral expansion.

## Non-Goals

- **GLX / DRI3 / Present completion** — Phase 4 territory (accelerated
  clients). Out of scope here.
- **MIT-SHM extension v1.2 features** — segment FD passing for shmseg
  identifiers, MIT-SHM PutImage-as-server-snapshot. We implement the
  subset wmaker actually uses.
- **Full XKEYBOARD synthesis** — separate work.
- **RENDER full coverage** — only the two known XID-attribute drops.
  Anything else surfacing during validation is filed as Phase 3.6.
- **Damage on text glyphs / RENDER ops** — first cut accumulates
  damage on core drawing ops only (PolyLine through PutImage).
  RENDER drawing damage is a follow-up if a real client needs it.

## Validation Targets

In order:

1. **Regression set:** `gtk3-demo`, `xeyes`, `xclock`, `xterm`, fvwm3
   startup, e16 startup, wmaker startup. Must continue to pass after
   each item lands.
2. **wmaker re-validation (item 1):** appicons should now show their
   icon graphic (xterm and xclock default icons) — this is the
   smoking-gun test.
3. **Compositor smoke (items 2 + 3):** run `picom -b` or equivalent
   under ynest with a transparent xterm. Compositor should:
   - call `RedirectSubwindows(root, automatic)` and
     `NameWindowPixmap(window)`,
   - subscribe to `DamageCreate` per top-level,
   - receive `DamageNotify` whenever the client draws.
   With (3) it doesn't crash; with (2) it actually composites motion.
4. **Xft clipping smoke (item 4):** any client using Xft to draw
   clipped text — `xclock`'s digital mode, GTK3 dialog labels with
   `text-overflow: ellipsis`. Visual diff should match Xephyr.

## MIT-SHM (item 1)

### Background

MIT-SHM lets clients pass image bytes by sharing a memory segment with
the X server, avoiding the round-trip cost of `PutImage`/`GetImage`.
Two transport modes exist:

- **`shmid` mode** (legacy SysV shm): client passes `shmget` ID, server
  attaches via `shmat`.
- **`fd` mode** (extension v1.2): client passes a POSIX shared-memory
  file descriptor over the X11 socket via `SCM_RIGHTS`, server
  `mmap`s it.

We implement **fd mode** only. SysV shm requires `IPC_PRIVATE` and the
client and server to share a kernel namespace, which doesn't work in
sandboxed setups. Fd mode is the modern path; both wmaker (libwraster)
and Xlib's `xcb-shm` use fd mode when available.

### Wire surface

The protocol surface is small. After `QueryVersion` the relevant
requests are:

| Minor | Name             | Cost |
|-------|------------------|------|
|   0   | QueryVersion     | reply with `(major=1, minor=2, uid=0, gid=0, pixmap_format=ZPixmap, shared_pixmaps=true)` |
|   1   | Attach           | takes `shmseg`, `shmid`, `read_only` — we reject (BadValue), we don't support legacy shmid mode |
|   3   | PutImage         | params + `shmseg`, `offset` — copy from segment to drawable |
|   4   | GetImage         | params + `shmseg`, `offset` — copy from drawable to segment, send reply with size |
|   5   | CreatePixmap     | takes `shmseg`, `offset`, dims, depth — register a pixmap whose pixels live in the segment |
|   6   | AttachFd (v1.2)  | takes `shmseg`, `read_only` and reads an fd via SCM_RIGHTS |
|   7   | CreateSegment    | server-allocated segment, sends fd back via SCM_RIGHTS |
|   2   | Detach           | release the segment |

Minimum subset wmaker requires: `QueryVersion`, `AttachFd`, `Detach`,
`CreatePixmap`. Optional: `PutImage` (for fast-path direct uploads),
`GetImage`. We implement all of `QueryVersion`, `AttachFd`, `Detach`,
`CreatePixmap`, `PutImage`, `GetImage` — they're all small. We
explicitly reject legacy `Attach` (no SysV shm).

### State and types

Add to `ServerState`:

```rust
pub mit_shm_segments: HashMap<u32 /* shmseg */, MitShmSegment>,
```

Where `MitShmSegment` carries:

```rust
struct MitShmSegment {
    owner: ClientId,
    file: std::fs::File,            // owns the lifetime of the FD
    mapping: MitShmMapping,         // see below — read-only or read/write
    size: usize,
    read_only: bool,
}

enum MitShmMapping {
    Read(memmap2::Mmap),
    ReadWrite(memmap2::MmapMut),
}
```

When the client's `AttachFd` request flag is `read_only=true`, we use
`Mmap::map(&file)`; otherwise `MmapMut::map_mut(&file)`. `GetImage`
needs the writable mapping (it copies drawable bytes into the
segment). `PutImage`/`CreatePixmap` need only read access. The match
on `mapping` happens at access time. We use `memmap2` rather than raw
`libc::mmap` to avoid manual unsafe.

### Pixmap-from-segment

`MIT-SHM CreatePixmap` registers a pixmap whose pixel storage *is* the
shm segment. The X spec requires the server to observe later writes
into the segment (that's the whole point of the extension — clients
keep a long-lived shm pixmap and update it across many draws). So we
**advertise `shared_pixmaps=false`** in the `QueryVersion` reply.

A `false` reply tells the client: the server received your shm
segment, but pixmaps backed by it won't track later mutations.
Toolkits (Xlib, xcb-shm, Qt, GTK) check this flag and fall back to
re-uploading via `MIT-SHM PutImage` for each frame instead of
expecting the pixmap to stay live. wmaker's flow (compose into shm,
CreatePixmap, CopyArea immediately, FreePixmap) doesn't depend on
shared-pixmap liveness and works either way.

Implementation:

- On `MIT-SHM CreatePixmap`, allocate a regular host pixmap of the
  requested dims/depth and copy the current segment contents into it
  via `host.put_image(...)`. Store the host xid on the local pixmap
  entry. From then on the local pixmap behaves like any other.
- On subsequent `CopyArea`/`PutImage`/`RENDER::Composite` etc., the
  forwarder uses the host xid as normal — no re-snapshot is needed
  (and would be wrong-by-spec to do anyway given we advertise
  `shared_pixmaps=false`).

If a real client later requires `shared_pixmaps=true` semantics, the
follow-up is the "mirror on every access" path: re-`put_image` from
the segment whenever the pixmap is used as a source. Defer until a
client actually needs it.

### Receive-fd plumbing

Each client thread owns a `UnixStream`. Reads pull bytes via
`read(2)`; for SCM_RIGHTS we need `recvmsg(2)` with a `cmsg` buffer.
`std::os::unix::net::UnixStream::recv_msg` doesn't exist, so we
either:

- use the `nix` crate's `recvmsg` wrapper (one new dep, well-trod), or
- call `libc::recvmsg` directly (no new dep but ~50 lines of unsafe).

Either is fine. The `nix` crate is already used or close-to-used in
many Rust X-related projects; we pick that for safety.

The hook point: `read_request` in the protocol crate currently does a
plain `read_exact`. For `MIT-SHM AttachFd` the request body is 12
bytes long but the FD is in the cmsg attached to the message that
delivered those bytes. We need to detect "next request is AttachFd"
and switch to `recvmsg` for that read. The cleanest model:

- Always read with `recvmsg` (fall back to `recv` if no FD arrived).
- Stash any received FD in a small per-client queue.
- When the dispatch sees `AttachFd`, pop the next FD from the queue
  and bind it to the `shmseg` ID in the request body.

This is the pattern Xorg uses internally and it composes correctly
with batched requests.

### Failure mode if MIT-SHM is unavailable

We always advertise MIT-SHM after this lands; there's no host
side-effect (we don't proxy MIT-SHM to the host — see below). Clients
that don't know it just don't use it.

### Why we don't proxy to the host

MIT-SHM pixmaps live in shared memory between the *client* and the
*X server*. Forwarding through ynest means:

- Client → ynest: works (we receive the fd via SCM_RIGHTS).
- ynest → host: would need to either re-send the fd to the host (not
  always possible if the host is on another machine or namespace) or
  buffer-copy from the shm segment to a regular host PutImage.

We do the latter — bytes-buffer-copy. wmaker's flow is exactly:
`PutImage(shm) → CopyArea(shm-pixmap, server-pixmap)` → host
*regular* CopyArea on a host-backed pixmap. Our materialisation step
handles this: shm-segment bytes get `host.put_image(...)`'d into a
fresh host pixmap, and from there everything is regular.

### Test surface

- Unit tests for the segment-table lifecycle (Attach, Detach, leak on
  client disconnect).
- Encoder tests for `QueryVersion` reply, `CreateSegment` reply (with
  fd), `GetImage` reply.
- Integration test that drives `MIT-SHM AttachFd` with a `memfd`
  passed via SCM_RIGHTS, then `CreatePixmap`, then verifies that a
  subsequent `CopyArea(shm-pixmap, real-pixmap)` produces the right
  bytes on the host side. (Use a `tempfile::tempfile()` for the fd
  source; map a known pattern; copy; check.)
- Manual: wmaker run shows real icon graphics in appicons.

## DAMAGE auto-accumulation (item 2)

### Background

A `DamageCreate` request creates a damage object that tracks "the area
of `drawable` that has been modified since the last `Subtract`". The
server is supposed to OR every drawing op's bounding rect into every
attached damage object's region.

Today we accept `DamageCreate` (storing the object) and `DamageAdd`
(explicitly updating the region) but never *fire* on drawing ops. So
clients see no `DamageNotify` events.

### Plan

1. Build a small `damage::accumulate(server, drawable, x, y, w, h)`
   helper that, for any damage object whose `drawable` is `drawable`
   (or a parent — see "drawable matching" below), unions the rect
   into the damage region and fires a `DamageNotify` if the level
   permits.
2. Call it from every core drawing op that produces visible output:
   `PolyLine`, `PolySegment`, `PolyRectangle`, `PolyArc`, `FillPoly`,
   `PolyFillRectangle`, `PolyFillArc`, `PutImage`, `CopyArea`,
   `CopyPlane`, `ImageText8`, `ImageText16`, `PolyText8`,
   `PolyText16`, `ClearArea`.
3. The bounding rect for ops with explicit dst coords (`CopyArea`,
   `PutImage`, `ClearArea`) is exact. For `PolyLine`/`PolySegment`/
   `PolyArc` we union the bounding box of each segment list (already
   computable from the request payload).
4. `RENDER::Composite`, `RENDER::CompositeGlyphs*`,
   `RENDER::FillRectangles`: skipped in this pass — Phase 3.6 if
   needed.

### Drawable matching

`DamageCreate` takes a single drawable, but the spec says damage on
*any descendant* should accumulate to the parent's damage object too
when level is `DamageReportRawRectangles` or
`DamageReportDeltaRectangles`. The four levels are:

```
0 = DamageReportRawRectangles  — fires on every op
1 = DamageReportDeltaRectangles — fires once per Subtract cycle
2 = DamageReportBoundingBox    — single rect per cycle
3 = DamageReportNonEmpty       — at-most-one event per cycle
```

For the first cut we implement levels 1, 2, 3. Level 0 (raw) requires
firing per-op; we defer it. Level 3 (non-empty) is the most common
for compositors; level 1 and 2 are used by accessibility tooling.

We match damage objects by walking the resource tree from the
modified drawable to root, and for each ancestor checking whether any
damage object's `drawable` matches. (Damage on a child window's
drawable accumulates to a damage object created on its parent.)

### `DamageNotify` event

Encoded inline (no new helper needed if we add a small encoder in
`yserver-protocol/src/x11/damage.rs`):

```
1   CARD8  type = first_event + 0
1   CARD8  level (the level the damage object was created with)
2   CARD16 sequence
4   CARD32 drawable
4   CARD32 damage
4   CARD32 timestamp
2,2,2,2  area      (x, y, width, height)  the rect that just damaged
2,2,2,2  geometry  (x, y, width, height)  drawable's full extent
4        more (CARD32 bool — true if more events coming this cycle)
```

That's 32 bytes — fits a normal X11 event.

### State changes

`DamageObject` already exists in `server.rs`; add a `region: Vec<Rect>`
and a `pending_event: bool` flag.

### Test surface

- Unit test that `accumulate` unions into the right damage object's
  region, fires `DamageNotify` for level 1/2/3, suppresses duplicates
  per cycle for level 3.
- Integration test: drive `PolyFillRectangle` against a window that
  has a damage object, read back a `DamageNotify` event from the
  client writer.
- Manual: `picom -b --backend=xrender` plus a transparent xterm shows
  motion compositing without lag; compositor logs no damage-related
  warnings.

## COMPOSITE `NameWindowPixmap` (item 3)

### Background

A real compositor (picom, mutter) flow is:

```
RedirectSubwindows(root, automatic)
for each top-level w in QueryTree(root):
    pixmap_for_w = NameWindowPixmap(w)
    DamageCreate(window=w, level=NonEmpty)
on DamageNotify(w):
    composite(pixmap_for_w, ...)
```

`NameWindowPixmap` returns a *named* `Pixmap` resource that points at
the window's off-screen storage created by `RedirectSubwindows`. The
pixmap is invalidated and re-created when the window is resized.

Today we return `BadMatch` for any `NameWindowPixmap` call; that
breaks the compositor flow at step 1.

### Plan

The key realisation is that **we have to actually use the host's
COMPOSITE extension end-to-end**. Allocating a fresh empty host
pixmap and calling it the "composite pixmap" doesn't work — the
client expects the pixmap to track the window's content, and only
the host's redirected backing store does that.

Flow:

1. On `RedirectSubwindows(root, mode)` or `RedirectWindow(window,
   mode)`, forward the same request to the host targeting the
   corresponding host top-level subwindow's parent (or the host
   container itself, for root-redirection). Track which client
   owns the redirect on `ServerState::composite_redirects` (already
   present).
2. `NameWindowPixmap(window, pixmap)`:
   - Validate that the client owns the new pixmap xid and that the
     window is host-backed and currently redirected. If not, return
     `BadMatch` (the existing behaviour for unredirected windows is
     spec-correct — keep it).
   - Allocate a fresh *host* pixmap xid via `host.allocate_xid()`.
   - Forward `Composite::NameWindowPixmap(host_window_xid, host_pix_xid)`
     to the host so the host populates the host pixmap with its own
     redirected backing store.
   - Register the local `Pixmap` resource with `host_xid =
     Some(host_pix_xid)` so subsequent `CopyArea` etc. forward
     correctly.
3. Track each named pixmap on the `Window` struct as a list:
   `composite_named_pixmaps: Vec<NamedCompositePixmap>` where
   `NamedCompositePixmap { client_pixmap, host_pixmap, width, height }`.
   `NameWindowPixmap` is callable multiple times per window — each
   call returns a distinct pixmap, and a window resize invalidates
   *all* previously named pixmaps on that window simultaneously.
4. On `ConfigureWindow` that changes the window's width or height,
   free every entry in `composite_named_pixmaps` on both the local
   and host sides, and clear the list. (The client is responsible for
   re-issuing `NameWindowPixmap` after a resize per the COMPOSITE
   spec.)
5. On `DestroyWindow` (or any cleanup that drops `Window`), free all
   named pixmaps the same way.

### Host capability fallback

If the host doesn't advertise COMPOSITE at all, we cannot satisfy
`NameWindowPixmap` faithfully — there's no host-side backing store to
alias. In that case, return `BadAlloc` from `NameWindowPixmap`
(per X.org's behaviour when redirection backing fails). Returning a
window-as-pixmap alias is unsafe: downstream code in compositors
treats the result as a real `Pixmap` resource and may issue
pixmap-only requests against it, getting `BadPixmap`/resource-type
mismatches when those reach the host.

Compositors that hit `BadAlloc` will gracefully degrade (typically
they just skip that window's compositing for the cycle).

### Host capability detection

In `HostX11::open_from_env`, after RENDER and SHAPE detection, probe
COMPOSITE: `QueryExtension("Composite")`. Cache its major opcode and
call `Composite::QueryVersion` over the host stream to learn the
version. If absent, `NameWindowPixmap` returns `BadAlloc` (see
above).

### Test surface

- Unit test that `NameWindowPixmap` on a redirected window returns a
  pixmap with a usable `host_xid`, and that subsequent `CopyArea` on
  the named pixmap goes to the host with the right xid.
- Unit test that **two** `NameWindowPixmap` calls on the same window
  return distinct pixmaps and that resizing the window invalidates
  **both** at once.
- Unit test that `NameWindowPixmap` on a window when the host lacks
  COMPOSITE returns `BadAlloc` (mock the host opcode as `None`).
- Integration: the existing composite tests, extended to drive
  `NameWindowPixmap` and verify a non-`BadMatch` reply.
- Manual: `picom -b --backend=xrender` runs without aborting at the
  `NameWindowPixmap` step. Verify by toggling a transparent xterm
  that the composited result tracks the window's actual content,
  *not* a stale or empty buffer (confirms the host-side redirect is
  actually populating the named pixmap).

## RENDER `ChangePicture` XID attributes (item 4)

### Background

`change_picture_safe_to_forward` currently rejects any `ChangePicture`
with `CPClipMask` or `CPAlphaMap` set to a non-`None` XID. Real Xft
text rendering with clip rects, and drop-shadow/glow effects via alpha
maps, both hit this path.

### Plan

For `CPClipMask = pixmap`:

1. Translate the client pixmap XID to its host XID via the existing
   `pixmap_host_xid` map.
2. If the pixmap has no host backing, log at DEBUG and drop (current
   behaviour for unsupported pixmap depths). This is rare — the clip
   mask is depth-1 and depth-1 pixmaps *are* host-backed.
3. Patch the `ChangePicture` body in-place: replace the client xid in
   the `values` slice with the host xid.
4. Forward via `host.render_change_picture(host_pic, body)`.

For `CPAlphaMap = picture`:

1. Translate the client picture XID via the existing `pictures` map.
2. Same drop-on-missing rule.
3. Patch + forward.

The encoder already accepts arbitrary `values` bytes so this is a
parsing-and-byte-substitution exercise, not a wire-format expansion.

### Test surface

- Unit test that `change_picture_safe_to_forward` returns `true` when
  `CPClipMask` is set with a known pixmap, and that the patched body
  carries the host pixmap xid.
- Unit test that `CPAlphaMap` with an unknown picture xid is dropped
  (returns the existing safe-to-forward = false to avoid a host-side
  `BadPicture`).
- Manual: GTK3 dialog with shadow-text labels, or `xclock -d`
  (digital), renders text without artefacts.

## Order and Build Sequence

1. **MIT-SHM** first — biggest user-visible win (wmaker icons), and
   the DAMAGE accumulation work below benefits from being able to
   trigger drawing via SHM-fast-paths in tests.
2. **RENDER `ChangePicture`** — small, isolated, and unblocks any GTK
   theme that tests it.
3. **DAMAGE auto-accumulation** — touches every drawing op handler
   but each touch is one `accumulate(...)` call. Best done all at
   once.
4. **COMPOSITE `NameWindowPixmap`** — depends on (3) being live so
   compositor smoke tests work end-to-end.

Each lands as its own commit on `phase3.5`.

## Risks

- **MIT-SHM fd plumbing.** The SCM_RIGHTS dance is the most novel
  part. Mitigation: write the `recvmsg` wrapper as a small standalone
  function with a unit test that round-trips a `memfd` fd through a
  socketpair before any of the MIT-SHM extension work depends on it.
- **DAMAGE rect computation for poly ops.** Computing the bounding
  box of a `PolySegment`/`PolyArc` payload is straightforward but
  easy to off-by-one. Mitigation: union the *whole drawable extent*
  on the first call as a worst-case fallback; tighten if the
  compositor smoke test shows flicker.
- **MIT-SHM `shared_pixmaps=false`.** We advertise that shm pixmaps
  do not track later segment writes. A toolkit that ignores this flag
  and assumes shared-pixmap liveness will see stale rendering. The
  validation plan must include at least one client that updates a
  shm segment *after* CreatePixmap and verify it falls back to
  `MIT-SHM PutImage` rather than re-using the stale pixmap. Qt is the
  canonical such client; smoke against `qt5-demos` or similar.
- **COMPOSITE host-side redirect ordering.** We forward
  `RedirectSubwindows`/`RedirectWindow` to the host before tracking
  the redirect locally. If the host fails the redirect (e.g., because
  another client already redirected the same target), our local
  bookkeeping says we redirected and we then issue `NameWindowPixmap`
  to a host that hasn't redirected. Mitigation: forward the redirect
  with a sync round-trip and only update local state on success.
- **NameWindowPixmap host pixmap leak on client crash.** If a client
  calls `NameWindowPixmap` and crashes before `FreePixmap`, we leak
  the host pixmap. Mitigation: free composite-named pixmaps in the
  per-client cleanup path, alongside the existing
  `background_pixmap_host_xid` cleanup.

## Testing Strategy

### Unit tests

- MIT-SHM: segment lifecycle, encoder round-trips, fd-receive helper.
- DAMAGE: accumulate union math, level gating, ancestor walk.
- COMPOSITE: NameWindowPixmap success/fail paths, resize invalidation.
- RENDER: ChangePicture body patching for CPClipMask/CPAlphaMap.

### Manual validation

Per-item checklist in the Order section; documented in `status.md`
as each lands.

### Build gate

```sh
cargo +nightly fmt
cargo clippy --workspace
cargo test --workspace
```

After every commit.

## Done Criteria

Phase 3.5 is done when:

1. wmaker appicons show their default icon graphic under `ynest`.
2. A no-config `picom -b --backend=xrender` run under `ynest` doesn't
   crash and composites a transparent xterm correctly.
3. GTK3 dialog with Xft clipped text renders without artefacts.
4. The existing regression set (`gtk3-demo`, `xeyes`, `xclock`,
   `xterm`, fvwm3, e16, wmaker) still passes.
5. `status.md` reflects the new opcode-table state, the MIT-SHM
   advertisement, and any deferred follow-ups (Phase 3.6 bullets).
