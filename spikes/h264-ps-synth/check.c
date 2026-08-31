/* The other two halves: does the slice parser find the id the stream really uses, and does
 * the Annex-B -> AVCC re-framing preserve every NAL exactly? */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "virgl_video_h264_ps.h"

static uint8_t *slurp(const char *p, size_t *len)
{
    FILE *f = fopen(p, "rb"); if (!f) { perror(p); exit(1); }
    fseek(f, 0, SEEK_END); *len = ftell(f); fseek(f, 0, SEEK_SET);
    uint8_t *b = malloc(*len);
    if (fread(b, 1, *len, f) != *len) { fprintf(stderr, "short read\n"); exit(1); }
    fclose(f); return b;
}

int main(int argc, char **argv)
{
    size_t len; unsigned id; int fail = 0;
    if (argc < 2) return 2;
    uint8_t *buf = slurp(argv[1], &len);

    if (virgl_h264_slice_pps_id(buf, len, &id)) { printf("FAIL: no slice found\n"); fail = 1; }
    else printf("%s: slice pic_parameter_set_id = %u (expect 0)\n", argv[1], id);
    if (id != 0) fail = 1;

    ssize_t need = virgl_h264_annexb_to_avcc(buf, len, NULL, 0);
    uint8_t *avcc = malloc(need);
    ssize_t got = virgl_h264_annexb_to_avcc(buf, len, avcc, need);
    printf("annexb %zu bytes -> avcc %zd bytes (sized %zd)\n", len, got, need);
    if (got != need) { printf("FAIL: size mismatch\n"); fail = 1; }

    /* Walk the AVCC output and check every NAL length lands exactly on the next one. */
    size_t off = 0, nals = 0;
    while (off + 4 <= (size_t)got) {
        size_t n = ((size_t)avcc[off] << 24) | ((size_t)avcc[off+1] << 16) |
                   ((size_t)avcc[off+2] << 8) | avcc[off+3];
        if (n == 0 || off + 4 + n > (size_t)got) { printf("FAIL: bad NAL length %zu at %zu\n", n, off); fail = 1; break; }
        off += 4 + n; nals++;
    }
    if (off != (size_t)got) { printf("FAIL: trailing %zu bytes\n", (size_t)got - off); fail = 1; }
    else printf("avcc walk: %zu NALs, exact fit\n", nals);

    /* A buffer one byte too small must be refused, not truncated. */
    if (virgl_h264_annexb_to_avcc(buf, len, avcc, need - 1) != -1) { printf("FAIL: overflow not caught\n"); fail = 1; }
    else printf("undersized output refused\n");

    printf(fail ? "RESULT: FAIL\n" : "RESULT: PASS\n");
    return fail;
}
