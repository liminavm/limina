// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// M7 USB-passthrough spike: can a plain userspace process (no DriverKit / no special
// entitlement) OPEN, CONFIGURE, and CLAIM a target USB device on macOS via libusb?
//
// This is the gating risk for USB/IP passthrough (research/06-usb-passthrough.md §1.5):
// macOS lets you claim devices with *no matching Apple driver* freely, but Apple-bound
// interfaces (mass storage, standard HID, audio) need DriverKit + restricted entitlements.
// This probe classifies a real device empirically.
//
// Build+run:  spikes/usb-probe/run.sh [VID PID]   (default 1209 beee = SoloKeys Solo 2)
//
// It enumerates everything, then for the target: opens it, reads device+config descriptors,
// lists interfaces/endpoints, and ATTEMPTS set_configuration + claim_interface on each
// interface — reporting the exact libusb error per step. A clean claim ⇒ this device is a
// v1 passthrough target with zero entitlement.

#include <libusb.h>  // pkg-config --cflags adds the libusb-1.0 include dir
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>  // geteuid

static const char *cls_name(uint8_t c) {
    switch (c) {
        case 0x00: return "(per-interface)";
        case 0x02: return "CDC-comm";
        case 0x03: return "HID";
        case 0x08: return "Mass-Storage";
        case 0x0a: return "CDC-data";
        case 0x0b: return "Smart-Card/CCID";
        case 0xe0: return "Wireless";
        case 0xef: return "Misc/IAD";
        case 0xfe: return "App-specific";
        case 0xff: return "Vendor-specific";
        default:   return "?";
    }
}

int main(int argc, char **argv) {
    unsigned want_vid = 0x1209, want_pid = 0xBEEE;  // SoloKeys Solo 2
    if (argc >= 3) { want_vid = strtoul(argv[1], 0, 16); want_pid = strtoul(argv[2], 0, 16); }

    libusb_context *ctx = NULL;
    int r = libusb_init(&ctx);
    if (r < 0) { fprintf(stderr, "libusb_init: %s\n", libusb_error_name(r)); return 1; }

    libusb_device **list = NULL;
    ssize_t n = libusb_get_device_list(ctx, &list);
    printf("== libusb sees %zd device(s) ==\n", n);

    libusb_device *target = NULL;
    struct libusb_device_descriptor td;
    for (ssize_t i = 0; i < n; i++) {
        struct libusb_device_descriptor d;
        if (libusb_get_device_descriptor(list[i], &d) < 0) continue;
        int is = (d.idVendor == want_vid && d.idProduct == want_pid);
        printf("  %04x:%04x  class=0x%02x %-16s %s\n", d.idVendor, d.idProduct,
               d.bDeviceClass, cls_name(d.bDeviceClass), is ? "  <-- TARGET" : "");
        if (is) { target = list[i]; td = d; }
    }

    if (!target) {
        printf("\nTarget %04x:%04x NOT found by libusb.\n", want_vid, want_pid);
        printf("(If ioreg shows it but libusb doesn't, that itself is the finding: macOS is\n"
               " hiding the device from userspace libusb.)\n");
        libusb_free_device_list(list, 1); libusb_exit(ctx); return 2;
    }

    printf("\n== TARGET %04x:%04x — descriptors ==\n", want_vid, want_pid);
    printf("  bDeviceClass=0x%02x %s  bcdUSB=0x%04x  bcdDevice=0x%04x  bNumConfigurations=%u\n",
           td.bDeviceClass, cls_name(td.bDeviceClass), td.bcdUSB, td.bcdDevice, td.bNumConfigurations);

    libusb_device_handle *h = NULL;
    r = libusb_open(target, &h);
    printf("\n[step] libusb_open: %s\n", r == 0 ? "OK" : libusb_error_name(r));
    if (r < 0) {
        printf("  -> macOS denied open from userspace. Likely needs an entitlement.\n");
        libusb_free_device_list(list, 1); libusb_exit(ctx); return 3;
    }

    // String descriptors (manufacturer/product/serial)
    unsigned char s[256];
    if (td.iProduct && libusb_get_string_descriptor_ascii(h, td.iProduct, s, sizeof s) > 0)
        printf("  iProduct      = \"%s\"\n", s);
    if (td.iManufacturer && libusb_get_string_descriptor_ascii(h, td.iManufacturer, s, sizeof s) > 0)
        printf("  iManufacturer = \"%s\"\n", s);
    if (td.iSerialNumber && libusb_get_string_descriptor_ascii(h, td.iSerialNumber, s, sizeof s) > 0)
        printf("  iSerial       = \"%s\"\n", s);

    int cfgnum = 0;
    libusb_get_configuration(h, &cfgnum);
    printf("  current configuration = %d\n", cfgnum);

    struct libusb_config_descriptor *cfg = NULL;
    r = libusb_get_active_config_descriptor(target, &cfg);
    if (r < 0) r = libusb_get_config_descriptor(target, 0, &cfg);
    if (r == 0 && cfg) {
        printf("\n== config #%u: %u interface(s), %u mA ==\n",
               cfg->bConfigurationValue, cfg->bNumInterfaces, cfg->MaxPower * 2);
        for (int i = 0; i < cfg->bNumInterfaces; i++) {
            const struct libusb_interface *iface = &cfg->interface[i];
            for (int a = 0; a < iface->num_altsetting; a++) {
                const struct libusb_interface_descriptor *id = &iface->altsetting[a];
                printf("  if %d.%d  class=0x%02x %-16s sub=0x%02x proto=0x%02x  %u endpoint(s)\n",
                       id->bInterfaceNumber, id->bAlternateSetting, id->bInterfaceClass,
                       cls_name(id->bInterfaceClass), id->bInterfaceSubClass,
                       id->bInterfaceProtocol, id->bNumEndpoints);
                for (int e = 0; e < id->bNumEndpoints; e++) {
                    const struct libusb_endpoint_descriptor *ep = &id->endpoint[e];
                    const char *type = (ep->bmAttributes & 3) == 0 ? "control" :
                                       (ep->bmAttributes & 3) == 1 ? "iso" :
                                       (ep->bmAttributes & 3) == 2 ? "bulk" : "interrupt";
                    printf("      ep 0x%02x %-9s %s  maxpkt=%u\n", ep->bEndpointAddress, type,
                           (ep->bEndpointAddress & 0x80) ? "IN " : "OUT", ep->wMaxPacketSize);
                }
            }
        }

        // The real test: can we CLAIM each interface? On macOS an Apple-bound interface needs the
        // driver DETACHED first — which requires either the (Apple-managed) com.apple.vm.device-access
        // entitlement OR **root**. So we try detach-then-claim and report both: run this plain to see
        // the userspace gate, and `sudo ./probe` to test the root-escalation path (the path a
        // Developer-ID app actually uses — no entitlement). NOTE macOS capture is DEVICE-level: a
        // detach on one interface seizes the whole device (all interfaces) at once.
        int running_as_root = (geteuid() == 0);
        printf("\n== claim test (the macOS gate)%s ==\n", running_as_root ? "  [running as ROOT]" : "");
        int any_apple_bound = 0, all_claimed = 1, needed_detach = 0;
        for (int i = 0; i < cfg->bNumInterfaces; i++) {
            int knl = libusb_kernel_driver_active(h, i);  // 1=bound, 0=free, <0=err/unsupported
            int det = 0;
            if (knl == 1) {
                needed_detach = 1;
                det = libusb_detach_kernel_driver(h, i);  // root or entitlement required on macOS
            }
            r = libusb_claim_interface(h, i);
            printf("  interface %d: kernel_driver_active=%s  detach=%s  claim=%s\n", i,
                   knl == 1 ? "YES(bound)" : knl == 0 ? "no" : libusb_error_name(knl),
                   knl == 1 ? (det == 0 ? "OK" : libusb_error_name(det)) : "n/a",
                   r == 0 ? "OK ✅" : libusb_error_name(r));
            if (r == 0) libusb_release_interface(h, i);
            else { all_claimed = 0; if (knl == 1) any_apple_bound = 1; }
        }
        libusb_free_config_descriptor(cfg);

        // Diagnose the M7 passthrough story for this device (the real cases).
        printf("\n== verdict ==\n");
        if (all_claimed && !needed_detach) {
            printf("  ✅ FREE-TO-CLAIM (unprivileged). open + every interface claim succeeded with no\n");
            printf("     driver detach. A confirmed M7 v1 target needing ONLY the USB TCC permission\n");
            printf("     (com.apple.security.device.usb) — no root, no Apple-granted entitlement.\n");
        } else if (all_claimed && needed_detach) {
            printf("  ✅ ROOT-CLAIMABLE. An Apple class driver held %s, but detach+claim SUCCEEDED%s —\n",
                   "interface(s)", running_as_root ? " (running as root)" : "");
            printf("     so real passthrough works via a small PRIVILEGED (root) USB-capture helper,\n");
            printf("     NO Apple entitlement. (macOS capture is DEVICE-level: seizing one interface\n");
            printf("     freed the others — the whole composite device is captured together, so the\n");
            printf("     host loses it while the guest holds it.) This is the M7 real-device path.\n");
        } else if (any_apple_bound) {
            printf("  ⚠️  APPLE-CLAIMED INTERFACES. open + descriptors work (the USB TCC permission is\n");
            printf("     granted), but a macOS class driver holds one or more interfaces, so claim →\n");
            printf("     EACCES. To seize it you must DETACH the Apple driver, which on macOS needs\n");
            printf("     either ROOT or the Apple-managed `com.apple.vm.device-access` entitlement.\n");
            if (running_as_root) {
                printf("     You ARE root but detach/claim still failed — investigate (SIP? device busy?).\n");
            } else {
                printf("     THE PRACTICAL PATH for a Developer-ID app: run the capture as ROOT (a small\n");
                printf("     privileged helper) — NO entitlement needed (Apple's documented fallback).\n");
                printf("     >>> Re-run `sudo %s` to confirm root claims it. <<<\n", "spikes/usb-probe/run.sh");
                printf("     (The entitlement itself is App-Store-only + Apple-approval-gated, and is\n");
                printf("     SIGKILLed at launch under ad-hoc signing — not a path for this app.)\n");
            }
        } else {
            printf("  ❓ open OK but claim failed for a non-driver reason (%s). Investigate.\n",
                   libusb_error_name(r));
        }
    } else {
        printf("  (could not read config descriptor: %s)\n", libusb_error_name(r));
    }

    libusb_close(h);
    libusb_free_device_list(list, 1);
    libusb_exit(ctx);
    return 0;
}
