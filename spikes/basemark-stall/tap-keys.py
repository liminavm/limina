# SPDX-License-Identifier: MIT
# Copyright © 2026 Gustavo Noronha Silva
#
# Tap named keys through /dev/uinput, so a headless capture can drive the shell.
#
#   sudo python3 tap-keys.py esc            leave the overview
#   sudo python3 tap-keys.py super esc      open it and leave again
#   sudo python3 tap-keys.py super:1.2 esc  tap, then wait 1.2s instead of the default
#
# A capture boots into the overview, which composites the session's windows as scaled thumbnails
# inside the shell's own UI. Those thumbnails are live and the compositor keeps presenting, so this
# is not the difference between a moving capture and a frozen one -- it is the difference between
# capturing the overview and capturing the workload at its own size. Escaping puts the focused
# window up, which is the shape a capture or a score is meant to be taken in.
# `type-into-overview.py` types words; this taps keys, which is the other half of the same need.
import fcntl, struct, sys, time, os

UI_DEV_CREATE, UI_DEV_DESTROY = 0x5501, 0x5502
UI_SET_EVBIT, UI_SET_KEYBIT = 0x40045564, 0x40045565
EV_SYN, EV_KEY, SYN_REPORT = 0, 1, 0

KEYS = {'esc': 1, 'super': 125, 'backspace': 14, 'enter': 28, 'tab': 15, 'space': 57,
        'f11': 87, 'up': 103, 'down': 108, 'left': 105, 'right': 106,
        'q': 16, 'w': 17, 'f': 33}

want = [a.split(':', 1) for a in sys.argv[1:]]
if not want:
    sys.exit("usage: tap-keys.py NAME[:PAUSE] ...  (%s)" % " ".join(sorted(KEYS)))
for name, *_ in want:
    if name not in KEYS:
        sys.exit("unknown key %r -- have %s" % (name, " ".join(sorted(KEYS))))

fd = os.open('/dev/uinput', os.O_WRONLY | os.O_NONBLOCK)
fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
for code in KEYS.values():
    fcntl.ioctl(fd, UI_SET_KEYBIT, code)
dev = b'virglrs-corpus-kbd'.ljust(80, b'\0') + struct.pack('HHHH', 0x03, 0x1234, 0x5678, 1) \
      + struct.pack('i', 0) + b'\0' * (64 * 4 * 4)
os.write(fd, dev)
fcntl.ioctl(fd, UI_DEV_CREATE)
time.sleep(2)   # let mutter/libinput adopt it

def ev(t, c, v):
    os.write(fd, struct.pack('qqHHi', 0, 0, t, c, v))

for name, *rest in want:
    code = KEYS[name]
    ev(EV_KEY, code, 1); ev(EV_SYN, SYN_REPORT, 0); time.sleep(0.03)
    ev(EV_KEY, code, 0); ev(EV_SYN, SYN_REPORT, 0)
    time.sleep(float(rest[0]) if rest else 0.8)

fcntl.ioctl(fd, UI_DEV_DESTROY)
os.close(fd)
print("tapped", " ".join(a for a, *_ in want))
