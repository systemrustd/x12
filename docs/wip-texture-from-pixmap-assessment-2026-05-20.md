# Texture-from-pixmap assessment — 2026-05-20

Context: while debugging COW-authoritative scanout and reparented tray
applets, we asked whether implementing GLX texture-from-pixmap would be
a small alternative path. Short answer: advertising the protocol surface
is small; making it actually work for yserver-owned redirected backings
is medium-to-large work.

## Scope split

There are two different levels of "texture from pixmap":

1. Advertise enough GLX attributes/extensions that clients probe further.
   This is probably a 0.5-1 day task.
2. Make compositors actually sample redirected window pixmaps as GL
   textures. This is the real feature: likely 1-2 weeks for a narrow MVP,
   and 3-6 weeks for robust Xorg-like behavior.

Do not fold this into the current COW-authoritative/reparent plan. It is
a separate phase.

## Current tree facts

- The GLX dispatcher already synthesizes FBConfigs and drawable
  attributes in `crates/yserver-core/src/core_loop/process_request.rs`.
- `drawable_attributes_for` reports the attributes Mesa commonly probes,
  including `GLX_TEXTURE_TARGET_EXT` and `GLX_Y_INVERTED_EXT`.
- The advertised GLX extension string currently does not include
  `GLX_EXT_texture_from_pixmap`.
- `VendorPrivate` / `VendorPrivateWithReply` requests are currently
  rejected as unsupported. Real TFP implementations often need these
  private GLX request paths for bind/release behavior.
- v2's DRI3 export path only exports pixmaps that were originally
  imported from a dma-buf. `dri3_export_pixmap` requires
  `drawable.storage.imported_drawable`.
- Normal yserver-owned pixmaps and redirected backing pixmaps are
  therefore not exportable to a client GL stack today.

## Main blocker

The hard part is not the GLX string. The client needs a GPU-importable
handle for the pixmap it wants to sample.

For COW/compositor use, the important pixmaps are usually yserver-owned
redirected backing images returned through `NameWindowPixmap`. Those are
Vulkan images in `DrawableStore`, but they are not currently allocated as
external-memory exportable images and cannot be handed to Mesa as dma-buf
textures.

So `NameWindowPixmap(panel)` can return a pixmap XID, but that does not
yet imply the client can bind that pixmap as a real GL texture.

## Work required for real TFP

Minimum real implementation pieces:

- Advertise `GLX_EXT_texture_from_pixmap` only when the backend can
  actually satisfy it.
- Extend FBConfig / drawable attributes with bind-to-texture fields such
  as RGB/RGBA texture support and texture target metadata.
- Track GLXPixmap resources correctly: creation, destruction, attributes,
  and mapping back to the underlying X pixmap / redirected backing.
- Implement or route the bind/release request path Mesa uses for
  texture-from-pixmap.
- Make yserver-owned pixmap storage exportable, either by allocating
  selected pixmaps/backings as external-memory-capable Vulkan images or
  by copying into an exportable image on demand.
- Add synchronization and lifetime handling so clients do not sample
  before yserver writes complete, and so `NameWindowPixmap` aliases keep
  the backing alive until the GL user releases it.
- Preserve damage correctness: if the backing contents are stale because
  reparent redirect reconciliation is wrong, TFP only exposes the same
  stale pixels through a different path.

## Recommendation

Finish the current COW-authoritative and reparent-redirect work first.
That aligns yserver with Xorg's compositor contract and fixes the
immediate tray/shadow class without adding a GL export surface.

After that, a useful next probe would be:

1. Add a temporary branch that advertises `GLX_EXT_texture_from_pixmap`
   and any missing harmless FBConfig attributes.
2. Trace marco/picom/Mesa to see exactly which GLX requests are issued.
3. Decide whether the first real implementation should use exportable
   server-owned pixmaps or an explicit copy-to-exportable-image bridge.

The cheap part is making clients ask. The expensive part is giving them
a real texture for yserver-owned redirected backings.
