#!/usr/bin/env python3
"""Tiny manual-validation probe for root ConfigureNotify on container resize.

Selects only StructureNotifyMask on the root window — no RANDR — and prints
every ConfigureNotify it receives, so we can verify ynest's
handle_host_container_resize fans out a core ConfigureNotify to non-RANDR
clients (panels and "fill the screen" apps).

Usage:
    DISPLAY=:99 python3 docs/root-resize-probe.py

Resize the ynest container in the host while this is running. Each resize
should print one line:
    ConfigureNotify(root=0x..., w=NEW_W, h=NEW_H)

Press Ctrl-C to exit.

Requires python3-xlib:
    pacman -S python-xlib   # or your distro equivalent
"""

from __future__ import annotations

import sys

try:
    from Xlib import X, display
except ImportError:
    sys.stderr.write(
        "python-xlib is not installed. Install it with:\n"
        "    pacman -S python-xlib   # Arch\n"
        "    apt install python3-xlib  # Debian/Ubuntu\n"
    )
    sys.exit(1)


def main() -> int:
    d = display.Display()
    root = d.screen().root
    root.change_attributes(event_mask=X.StructureNotifyMask)
    d.sync()
    print(f"Listening for ConfigureNotify on root 0x{root.id:x} ({d.get_display_name()})")
    print("Resize the ynest container in another window; Ctrl-C to exit.")
    try:
        while True:
            ev = d.next_event()
            if ev.type == X.ConfigureNotify:
                print(
                    f"ConfigureNotify(root=0x{ev.window.id:x}, "
                    f"x={ev.x}, y={ev.y}, w={ev.width}, h={ev.height})"
                )
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main())
