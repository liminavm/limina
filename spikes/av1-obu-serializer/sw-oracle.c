/* Drives the serializer into the SOFTWARE decoder the backend falls back to, the same way
 * vt-oracle drives it into VideoToolbox: unit by unit, in backend order.
 *
 * What it is here to pin is the wrapper's contract, which the backend's one-unit-one-target
 * model rests on: every unit the backend submits yields exactly one picture, hidden frames
 * included (dav1d emits only shown frames unless asked otherwise, and a hidden frame whose
 * surface is never written is invisible until a later show_existing_frame displays it).
 * Then it checksums the shown pictures so they can be compared against the reference decode.
 *
 * Usage: ./sw-oracle <capture-dir>
 */

#include <dav1d/dav1d.h>
#include <dirent.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "virgl_video_av1_obu.h"
#include "virgl_video_dav1d.h"

/* The wrapper calls this; the real one is a static inline in virgl_util.h, which cannot be
 * included outside a configured virglrenderer build. */
void virgl_error(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
}

#define MAX_FRAMES 512

static int cmp(const void *a, const void *b)
{
    return strcmp(*(const char *const *)a, *(const char *const *)b);
}

int main(int argc, char **argv)
{
    struct virgl_av1_obu_state state = {0};
    struct virgl_video_dav1d *sw;
    char *names[MAX_FRAMES];
    unsigned n = 0, units = 0, pictures = 0, hidden_missing = 0;
    uint8_t *unit = NULL;
    size_t unit_cap = 0;
    DIR *d;
    struct dirent *e;

    if (argc < 2) {
        fprintf(stderr, "usage: %s <capture-dir>\n", argv[0]);
        return 2;
    }

    if (!(d = opendir(argv[1]))) {
        perror(argv[1]);
        return 2;
    }
    while ((e = readdir(d)) && n < MAX_FRAMES) {
        size_t l = strlen(e->d_name);
        if (l > 5 && !strcmp(e->d_name + l - 5, ".desc"))
            names[n++] = strdup(e->d_name);
    }
    closedir(d);
    qsort(names, n, sizeof(*names), cmp);

    if (!(sw = virgl_dav1d_open())) {
        fprintf(stderr, "could not open the software decoder\n");
        return 1;
    }

    for (unsigned i = 0; i < n; i++) {
        struct virgl_av1_picture_desc desc;
        struct virgl_dav1d_picture pic;
        char path[1024];
        uint8_t *tiles = NULL;
        size_t tiles_size = 0;
        ssize_t need, got;
        FILE *f;
        int r;

        snprintf(path, sizeof(path), "%s/%.*s.desc", argv[1],
                 (int)(strlen(names[i]) - 5), names[i]);
        if (!(f = fopen(path, "rb")) || fread(&desc, sizeof(desc), 1, f) != 1) {
            fprintf(stderr, "cannot read %s\n", path);
            return 1;
        }
        fclose(f);

        snprintf(path, sizeof(path), "%s/%.*s.tile", argv[1],
                 (int)(strlen(names[i]) - 5), names[i]);
        if ((f = fopen(path, "rb"))) {
            fseek(f, 0, SEEK_END);
            tiles_size = (size_t)ftell(f);
            fseek(f, 0, SEEK_SET);
            tiles = malloc(tiles_size ? tiles_size : 1);
            if (tiles_size && fread(tiles, tiles_size, 1, f) != 1) {
                fprintf(stderr, "cannot read %s\n", path);
                return 1;
            }
            fclose(f);
        }

        /* Mirror the backend: flush whatever is held, then build this frame's unit. */
        need = virgl_av1_held_bound(&state) + VIRGL_AV1_UNIT_OVERHEAD;
        if ((size_t)need > unit_cap) {
            unit = realloc(unit, (size_t)need);
            unit_cap = (size_t)need;
        }
        got = virgl_av1_flush_held(&state, &desc, unit, unit_cap);
        if (got > 0) {
            units++;
            r = virgl_dav1d_decode(sw, unit, (size_t)got, &pic);
            if (r < 0) { fprintf(stderr, "held unit failed at %u\n", i); return 1; }
            if (r == 0) hidden_missing++;
            else { pictures++; virgl_dav1d_release(sw); }
        }

        need = (ssize_t)tiles_size + VIRGL_AV1_UNIT_OVERHEAD;
        if ((size_t)need > unit_cap) {
            unit = realloc(unit, (size_t)need);
            unit_cap = (size_t)need;
        }
        got = virgl_av1_build_temporal_unit(&state, &desc, tiles, tiles_size,
                                            unit, unit_cap);
        if (got < 0) { fprintf(stderr, "build failed at %u\n", i); return 1; }
        if (got > 0) {
            units++;
            r = virgl_dav1d_decode(sw, unit, (size_t)got, &pic);
            if (r < 0) { fprintf(stderr, "unit failed at %u\n", i); return 1; }
            if (r == 0) {
                hidden_missing++;
            } else {
                unsigned long long s = 0;
                for (uint32_t y = 0; y < pic.height; y++)
                    for (uint32_t x = 0; x < pic.width; x++)
                        s += pic.plane[0][(size_t)y * pic.pitch[0] + x];
                printf("  frame %2u %ux%u luma %016llx mean=%.4f\n", i,
                       pic.width, pic.height, s,
                       (double)s / ((double)pic.width * pic.height));
                pictures++;
                virgl_dav1d_release(sw);
            }
        }
        free(tiles);
    }

    virgl_dav1d_close(sw);
    printf("\n%u units -> %u pictures (%u yielded none)\n", units, pictures, hidden_missing);
    if (hidden_missing) {
        printf("FAIL: a unit produced no picture; the backend would leave that guest "
               "surface unwritten\n");
        return 1;
    }
    printf("PASS: every unit produced exactly one picture\n");
    return 0;
}
