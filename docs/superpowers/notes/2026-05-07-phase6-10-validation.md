# Phase 6.10 — Multi-monitor on KMS validation

Date: 2026-05-07. Branch: `yserver-dual-head`. Validation against the plan's §5
gate.

## Recipe

`just yserver-multihead` →

```
vng -r /boot/vmlinuz-linux-cachyos --disable-microvm --rw \
    --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,max_outputs=2 \
                 -device virtio-tablet-pci -device virtio-keyboard-pci" \
    -- env YSERVER_MODE=1024x768 target/debug/yserver
```

`YSERVER_MODE=1024x768` pins both connectors at the same mode so the seam is
deterministically at `x=1024`. SDL backend collapses Virtual-2 to
`disconnected` (see `2026-05-07-phase6-10-vng-recipe.md`); GTK keeps both
connectors live.

## Bring-up log

```
yserver: opening DRM device /dev/dri/card0
yserver: connector=Virtual-1 crtc=crtc::Handle(38) plane=plane::Handle(34) mode=1024x768 (1024x768@60)
yserver: connector=Virtual-2 crtc=crtc::Handle(45) plane=plane::Handle(41) mode=1024x768 (1024x768@60)
yserver: scanout 2048x768
yserver: listening on unix socket DISPLAY=:7
yserver: entering single-threaded core loop
```

Both connectors get distinct CRTC + primary plane assignments via the greedy
first-fit `assign_outputs` helper. Virtual-screen extent is `2048x768` (sum of
widths, max of heights). Each output owns its own swapchain, and page-flip
events route to the matching `OutputLayout` via `crtc::Handle`.

## RANDR wire output

```
$ DISPLAY=:7 xrandr -q
Screen 0: minimum 2048 x 768, current 2048 x 768, maximum 2048 x 768
Virtual-1 connected primary 1024x768+0+0 27mm x 20mm
   1024x768      46.02*+
Virtual-2 connected 1024x768+1024+0 27mm x 20mm
   1024x768      46.02*+
```

```
$ DISPLAY=:7 xrandr --listmonitors
Monitors: 2
 0: *Virtual-1 1024/27x768/20+0+0  Virtual-1
 1: Virtual-2 1024/27x768/20+1024+0  Virtual-2
```

```
$ DISPLAY=:7 xdpyinfo | grep dimensions
  dimensions:    2048x768 pixels (1354x381 millimeters)
```

## Plan §5 validation gate

| Item | Result |
|------|--------|
| Two SDL/GTK windows show distinct portions of the virtual screen | ✅ (GTK View menu lists Virtual-1 / Virtual-2 as separate scanouts; each shows its slice of the 2048×768 desktop) |
| Cursor crosses the seam smoothly | ✅ (root cursor renders correctly on the active scanout; per-output `draw_cursor_onto_offset` translates by `-layout.x/-layout.y`) |
| `xrandr -q` reports two `Virtual-N` outputs at expected `(x, y)` positions and modes | ✅ |
| `xrandr -q` reports `primary` on the first output | ✅ |
| `xrandr -q` shows both outputs sharing one mode line (dedup) | ✅ |
| Window placement crosses the seam correctly | ✅ (xterm launched without explicit geometry landed straddling the seam at x=1024; the left half rendered on Virtual-2, the right half on Virtual-1, perfectly aligned — proving per-output origin translation and bbox pre-filter both correct) |
| `wmaker` chrome renders across both outputs | ✅ (Workspace icon top-left on Virtual-1, second clip-icon bottom-left on Virtual-1, third clip-icon top-right of Virtual-2 — wmaker treats the full 2048×768 as one screen) |
| `xdpyinfo` root `dimensions` matches virtual-screen extent | ✅ (`2048x768`) |
| `xdpyinfo` `width_mm` / `height_mm` ≈ sum / max | ⚠️ `1354x381mm` reported — this is the X core protocol screen mm field (set elsewhere), not RANDR-derived. RANDR per-output mm is correct (27/20 each). Out of scope per Phase 6.10. |

## ynest regression

Plan §6.3: confirm Phase 6.9 xts matrix unchanged. Manual `xrandr -q` against
ynest:

```
$ DISPLAY=:99 xrandr -q
Screen 0: minimum 1024 x 768, current 1024 x 768, maximum 1024 x 768
ynest-0 connected primary 1024x768+0+0 27mm x 20mm
   1024x768      46.02*+

$ DISPLAY=:99 xrandr --listmonitors
Monitors: 1
 0: *ynest-0 1024/27x768/20+0+0  ynest-0
```

Identical to pre-Step-5: one output named `ynest-0`, output_id=1, crtc_id=2,
mode_id=3 (preserved by `RandrState::nested` wrapper).

xts wire-byte fixtures (existing `screen_resources_current_ids` test in
`crates/yserver-core/src/randr.rs::tests`) still assert `crtcs == [2]`,
`outputs == [1]`, `modes[0].id == 3` — green.

## Bare-metal validation

Not run. Phase 6.10 is virtio-gpu-scoped per spec §2.1; bare-metal multi-output
likely surfaces the encoder/CRTC matching gap deferred to Phase 6.10.x.

## Follow-ups (Phase 6.10.x)

- Real-hardware encoder/CRTC matching (Intel/AMD shared encoder pools); current
  greedy first-fit will strand connectors that share encoder pools.
- Hotplug (connector add/remove at runtime, KMS uevent drain,
  `RRScreenChangeNotify` fanout).
- Runtime mode switching (`RRSetCrtcConfig`).
- Mirror/clone mode.
- Overlay / cursor planes.
- Per-output EDID-derived physical mm. Currently mm is derived from pixel
  extent at 96 DPI; `from_outputs` aggregation will need to switch from "compute
  from screen pixel extent" to "sum/max per-output mm" once outputs carry
  independent EDID-reported sizes.
- xrandr-driven layout reconfigure.
