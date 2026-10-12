# hid-mouse — unaccelerated mouse counts for the guest-controlled pointer

Question: can limina read a mouse's own counts (no macOS pointer acceleration) while the pointer
is captured, so the guest's acceleration is the only one, and which devices must it leave out?

`hidprobe.swift` (`swiftc -O hidprobe.swift -o hidprobe && ./hidprobe [s] [--disassociate]
[--quiet]`) matches IOHIDManager on GenericDesktop Mouse and Pointer, lists each device and
counts X/Y/Wheel/AC Pan values and raw reports.

Measured 2026-10-11, M1 Max MacBook Pro, macOS 26.6.2, a Lenovo Legion M300 on USB, run from a
terminal that holds Input Monitoring:

- **Reports keep coming with the cursor disassociated** (`CGAssociateMouseAndMouseCursorPosition
  (false)`, as capture does): 5 861 mouse reports in 20 s detached.
- **They are the device's own counts.** 8-byte reports, report id 1, 16-bit X and Y; a fast
  flick peaks at 17–23 counts per report, at ~970 reports/s while moving (the mouse's USB
  polling). The wheel arrives as ±1 detents on GenericDesktop Wheel.
- **The built-in trackpad is a HID mouse too**: "Apple Internal Keyboard / Trackpad", SPI,
  primary usage Mouse, pairs `1:2, 1:1, ff00:25`, and it does **not** conform to Digitizer
  TouchPad. Detached, it delivered 503 X and 647 Y values from 804 raw 76-byte multitouch
  reports. A reader that takes every Mouse would move the guest twice for a finger, once on
  the touchpad and once on the relative mouse. The mouse has no `ff00` pair; the trackpad has
  no Wheel (`1:38`).
- **Matching both Mouse and Pointer makes two `IOHIDDevice` objects for one device**: both
  report the same registry entry id (`0x10001991c`), each with its own queue. Deduplicate by
  registry entry id.
- **The queue drops at ~1 kHz, slightly.** Quiet (no printing), the two objects of the same
  mouse saw 19 462 and 19 441 reports and X sums of 2 558 and 2 586: about 0.1% of the
  reports, which is invisible as pointer motion. A callback that prints per value drops far
  more (−514 against −185 in one run), so the reader must do nothing slow on its thread.
- `IOHIDCheckAccess(ListenEvent)` was `granted` and `IOHIDManagerOpen` succeeded here; whether
  the app bundle needs Input Monitoring for a mouse is answered by the app itself (it logs both
  at start).
