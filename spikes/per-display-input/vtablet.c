// vtablet — create virtual absolute input devices in the guest, to find out what a
// compositor does with more than one of them when there is more than one monitor.
//
// The question this spike exists to answer: limina wants one absolute pointing device per
// virtual display, so a click can land on the display the user aimed at without limina
// knowing how the guest arranged its monitors. Reading the compositors said only
// tablet-class and touch-class devices are ever bound to a single output — a pointer-class
// device is mapped over the whole desktop by mutter (meta-backend.c:490,
// meta-seat-impl.c:2471) and by KWin (connection.cpp:627,:388). This creates each class
// through uinput so the claim can be measured instead of inferred.
//
// uinput is the right vehicle because everything that decides the binding — the device
// name, the axis ranges, the physical size, the udev tags derived from the capability bits —
// is identical whether the events arrive over uinput or over virtio-input. What it does NOT
// reproduce is limina's real constraint that a virtio-input device's name and axes are
// config-space state read once at probe; that one is already known and needs no measurement.
//
// Build (in the guest):  cc -O2 -o vtablet vtablet.c
// Run:                   sudo ./vtablet --name 'limina LMN 0x31d7dd41' --mm 301x195
//
// Then type commands on stdin:
//   m <x> <y>   absolute move, in 0..65535 over the device's own extent
//   c           click (tip down + up for a tablet, touch for a touchscreen)
//   s <secs>    sweep left-right across the middle for <secs>, ~60 Hz
//   q           quit
//
// Keeping the process alive keeps the device alive; exiting removes it, which is how the
// hotplug half of the spike is driven. `--fifo <path>` reads commands from a named pipe and
// reopens it on EOF, so the device outlives each `echo > pipe` — the only way to drive it from
// a series of short ssh calls, since backgrounding a job inside an ssh command does not survive.

#include <errno.h>
#include <math.h>
#include <fcntl.h>
#include <linux/input.h>
#include <linux/uinput.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define ABS_RANGE 65535

enum kind { KIND_TABLET, KIND_TOUCHSCREEN, KIND_POINTER };

static void die(const char *what) {
    fprintf(stderr, "vtablet: %s: %s\n", what, strerror(errno));
    exit(1);
}

static void emit(int fd, int type, int code, int value) {
    struct input_event ev = {.type = (unsigned short)type,
                             .code = (unsigned short)code,
                             .value = value};
    if (write(fd, &ev, sizeof ev) != (ssize_t)sizeof ev)
        die("write");
}

static void syn(int fd) { emit(fd, EV_SYN, SYN_REPORT, 0); }

// Resolution is in units per mm, and it is the whole point of the size heuristic: mutter
// compares the device's derived physical size against the monitor's physical dimensions
// within a 10% tolerance (meta-input-mapper.c:371). A device that reports no resolution
// has no size, and match_size cannot fire for it at all.
static void setup_abs(int fd, int axis, int res) {
    struct uinput_abs_setup abs = {
        .code = (unsigned short)axis,
        .absinfo = {.minimum = 0, .maximum = ABS_RANGE, .resolution = res},
    };
    if (ioctl(fd, UI_ABS_SETUP, &abs) < 0)
        die("UI_ABS_SETUP");
}

int main(int argc, char **argv) {
    const char *name = "limina virtual tablet";
    const char *fifo = NULL;
    enum kind kind = KIND_TABLET;
    int width_mm = 0, height_mm = 0;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--name") && i + 1 < argc) {
            name = argv[++i];
        } else if (!strcmp(argv[i], "--fifo") && i + 1 < argc) {
            fifo = argv[++i];
        } else if (!strcmp(argv[i], "--mm") && i + 1 < argc) {
            if (sscanf(argv[++i], "%dx%d", &width_mm, &height_mm) != 2) {
                fprintf(stderr, "vtablet: --mm wants WxH in millimetres\n");
                return 2;
            }
        } else if (!strcmp(argv[i], "--kind") && i + 1 < argc) {
            const char *k = argv[++i];
            if (!strcmp(k, "tablet"))
                kind = KIND_TABLET;
            else if (!strcmp(k, "touchscreen"))
                kind = KIND_TOUCHSCREEN;
            else if (!strcmp(k, "pointer"))
                kind = KIND_POINTER;
            else {
                fprintf(stderr, "vtablet: --kind is tablet|touchscreen|pointer\n");
                return 2;
            }
        } else {
            fprintf(stderr, "vtablet: unknown argument '%s'\n", argv[i]);
            return 2;
        }
    }

    int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
    if (fd < 0)
        die("open /dev/uinput");

    if (ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0)
        die("UI_SET_EVBIT EV_KEY");
    if (ioctl(fd, UI_SET_EVBIT, EV_ABS) < 0)
        die("UI_SET_EVBIT EV_ABS");

    // The capability bits are what udev's input_id builtin reads to tag the device, and the
    // tag is what libinput classifies on. BTN_TOOL_PEN gets ID_INPUT_TABLET; BTN_TOUCH with
    // no pen and no INPUT_PROP_POINTER gets ID_INPUT_TOUCHSCREEN; INPUT_PROP_POINTER with
    // mouse buttons is a plain absolute pointer, which is exactly what limina ships today
    // and is included here as the control that should NOT bind to an output.
    switch (kind) {
    case KIND_TABLET:
        ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_PEN);
        ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH);
        ioctl(fd, UI_SET_KEYBIT, BTN_STYLUS);
        ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT);
        // A tablet tool has no wheel in libinput's tablet interface, but limina needs scroll
        // to land where the tool put the cursor — each device gets its OWN logical pointer, so
        // scroll arriving on some other device would apply at some other position. Declaring
        // the wheel here asks whether the axis survives the tablet classification at all.
        ioctl(fd, UI_SET_EVBIT, EV_REL);
        ioctl(fd, UI_SET_RELBIT, REL_WHEEL);
        ioctl(fd, UI_SET_RELBIT, REL_WHEEL_HI_RES);
        break;
    case KIND_TOUCHSCREEN:
        ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH);
        ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT);
        break;
    case KIND_POINTER:
        ioctl(fd, UI_SET_KEYBIT, BTN_LEFT);
        ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT);
        ioctl(fd, UI_SET_KEYBIT, BTN_MIDDLE);
        ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER);
        break;
    }

    if (ioctl(fd, UI_SET_ABSBIT, ABS_X) < 0)
        die("UI_SET_ABSBIT ABS_X");
    if (ioctl(fd, UI_SET_ABSBIT, ABS_Y) < 0)
        die("UI_SET_ABSBIT ABS_Y");
    if (kind == KIND_TABLET && ioctl(fd, UI_SET_ABSBIT, ABS_PRESSURE) < 0)
        die("UI_SET_ABSBIT ABS_PRESSURE");

    setup_abs(fd, ABS_X, width_mm > 0 ? ABS_RANGE / width_mm : 0);
    setup_abs(fd, ABS_Y, height_mm > 0 ? ABS_RANGE / height_mm : 0);
    if (kind == KIND_TABLET) {
        struct uinput_abs_setup p = {
            .code = ABS_PRESSURE,
            .absinfo = {.minimum = 0, .maximum = 1023},
        };
        if (ioctl(fd, UI_ABS_SETUP, &p) < 0)
            die("UI_ABS_SETUP pressure");
    }

    struct uinput_setup setup = {
        .id = {.bustype = BUS_VIRTUAL, .vendor = 0x1af4, .product = 0x1052, .version = 1},
    };
    snprintf(setup.name, sizeof setup.name, "%s", name);
    if (ioctl(fd, UI_DEV_SETUP, &setup) < 0)
        die("UI_DEV_SETUP");
    if (ioctl(fd, UI_DEV_CREATE) < 0)
        die("UI_DEV_CREATE");

    printf("vtablet: created '%s' (%s, %dx%d mm)\n", name,
           kind == KIND_TABLET        ? "tablet"
           : kind == KIND_TOUCHSCREEN ? "touchscreen"
                                      : "pointer",
           width_mm, height_mm);
    fflush(stdout);

    FILE *in = stdin;
    if (fifo) {
        in = fopen(fifo, "r");
        if (!in)
            die("open fifo");
    }

    char line[256];
    for (;;) {
        if (!fgets(line, sizeof line, in)) {
            if (!fifo)
                break;
            // A writer closed the pipe. Reopen and keep the device alive.
            fclose(in);
            in = fopen(fifo, "r");
            if (!in)
                die("reopen fifo");
            continue;
        }
        int x, y;
        if (sscanf(line, "m %d %d", &x, &y) == 2) {
            if (kind == KIND_TABLET) {
                emit(fd, EV_KEY, BTN_TOOL_PEN, 1);
            }
            emit(fd, EV_ABS, ABS_X, x);
            emit(fd, EV_ABS, ABS_Y, y);
            syn(fd);
            printf("moved %d %d\n", x, y);
        } else if (line[0] == 's') {
            // A tablet tool is only "in proximity" while it keeps reporting: libinput drops it
            // 50 ms after the last event even though we never send BTN_TOOL_PEN=0 (measured —
            // proximity-out arrived at +0.055s). A compositor only draws a tablet cursor during
            // proximity, so a sweep driven by one `echo` per step from a shell loop is invisible
            // however slowly it moves. Sweeping from inside the process is the only way to hold
            // the tool down long enough to see anything.
            int secs = 10;
            sscanf(line, "s %d", &secs);
            int steps = secs * 60;
            for (int i = 0; i < steps; i++) {
                double phase = (double)i / 60.0;
                int px = (int)((0.5 + 0.45 * sin(phase * 2.0)) * ABS_RANGE);
                emit(fd, EV_KEY, BTN_TOOL_PEN, 1);
                emit(fd, EV_ABS, ABS_X, px);
                emit(fd, EV_ABS, ABS_Y, ABS_RANGE / 2);
                syn(fd);
                usleep(16000);
            }
            emit(fd, EV_KEY, BTN_TOOL_PEN, 0);
            syn(fd);
            printf("swept %d s\n", secs);
        } else if (line[0] == 'w') {
            int clicks = 1;
            sscanf(line, "w %d", &clicks);
            emit(fd, EV_KEY, BTN_TOOL_PEN, 1);
            emit(fd, EV_ABS, ABS_X, ABS_RANGE / 2);
            emit(fd, EV_ABS, ABS_Y, ABS_RANGE / 2);
            syn(fd);
            emit(fd, EV_REL, REL_WHEEL_HI_RES, clicks * 120);
            emit(fd, EV_REL, REL_WHEEL, clicks);
            syn(fd);
            printf("wheel %d\n", clicks);
        } else if (line[0] == 'c') {
            int code = kind == KIND_POINTER ? BTN_LEFT : BTN_TOUCH;
            if (kind == KIND_TABLET)
                emit(fd, EV_ABS, ABS_PRESSURE, 800);
            emit(fd, EV_KEY, code, 1);
            syn(fd);
            usleep(60000);
            if (kind == KIND_TABLET)
                emit(fd, EV_ABS, ABS_PRESSURE, 0);
            emit(fd, EV_KEY, code, 0);
            syn(fd);
            printf("clicked\n");
        } else if (line[0] == 'q') {
            break;
        }
        fflush(stdout);
    }

    ioctl(fd, UI_DEV_DESTROY);
    close(fd);
    return 0;
}
