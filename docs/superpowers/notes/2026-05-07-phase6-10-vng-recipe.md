# Phase 6.10 — Verified vng multi-scanout recipe

Recorded: 2026-05-07. Supersedes the tentative recipe in
[`../specs/2026-05-07-phase6-10-multi-monitor-design.md`](../specs/2026-05-07-phase6-10-multi-monitor-design.md) §2.8.

## Verified recipe (GTK)

```
vng -r /boot/vmlinuz-linux-cachyos --disable-microvm --rw \
    --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,max_outputs=2 \
                 -device virtio-tablet-pci -device virtio-keyboard-pci" \
    -- target/debug/yserver
```

Kernel-reported connectors:

```
/sys/class/drm/card0-Virtual-1/status: connected
/sys/class/drm/card0-Virtual-2/status: connected
```

QEMU GTK exposes both scanouts as independently selectable entries under the
**View** menu. Switching between them shows the per-connector framebuffer.

## SDL collapses scanouts (do not use)

```
-display sdl,gl=on -vga none -device virtio-gpu-pci,max_outputs=2 ...
```

Under SDL the second connector reports `disconnected`:

```
/sys/class/drm/card0-Virtual-1/status: connected
/sys/class/drm/card0-Virtual-2/status: disconnected
```

This is the "kernel collapsing scanouts" failure mode anticipated by Step 0.2
of the plan. Likely SDL backend / virtio-gpu interaction; not investigated
further since GTK works.

## yserver bring-up smoke under chosen recipe (Step 0.4)

`target/debug/yserver` (current single-output backend) boots cleanly:

- modesets `Virtual-1` at `640x480@60` (preferred mode)
- `Virtual-2` remains blank-but-present in the GTK View menu
- root scanout visible on Virtual-1 (visually confirmed; no client → no
  window content, just the root fill — this is expected current behavior)
- SIGTERM → clean shutdown, master released

DRM init takes ~16s of guest time before "scanout" is logged; runs shorter
than that will appear blank.

## Caveats

- Host: CachyOS, kernel `linux-cachyos`. QEMU version: whatever vng pulls in
  on this host as of 2026-05-07.
- The two scanouts are *not* tiled into a single QEMU window — each is its
  own GTK tab. That's adequate for Phase 6.10 validation (xrandr reports
  layout, xterm geometry placement crosses the seam logically) but a real
  side-by-side compositor view requires a different host display backend.
- `max_outputs=2` is the only value tested. `max_outputs>2` is unverified.

## Used by

- Phase 6.10 plan Step 5 (codex self-review)
- Phase 6.10 plan Step 6 (`yserver-multihead` Justfile target & smoke gate)
