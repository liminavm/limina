# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# The serializer under test is MIT code in the virglrenderer fork, compiled straight from
# there rather than copied: a copy would drift, and the point of the oracle is to judge the
# thing that actually ships. dav1d is BSD-2 and stays on this side of the fence.
VIRGL   := ../../third_party/virglrenderer/src
CFLAGS  += -O2 -g -Wall -Wextra -Wno-unused-parameter -I$(VIRGL) -I$(VIRGL)/vrend -I$(VIRGL)/gallium/include
DAV1D_CFLAGS := $(shell pkg-config --cflags dav1d)
DAV1D_LIBS   := $(shell pkg-config --libs dav1d)

all: coverage oracle

# Needs AV1 silicon (M3 or later) and no dav1d, so it is built on demand rather than by
# `all`: on a machine without the decoder it builds fine and reports SKIP.
VT_LIBS := -framework CoreFoundation -framework CoreMedia -framework CoreVideo -framework VideoToolbox

coverage: coverage.c
	$(CC) $(CFLAGS) -o $@ $<

oracle: oracle.c $(VIRGL)/vrend/virgl_video_av1_obu.c
	$(CC) $(CFLAGS) $(DAV1D_CFLAGS) -o $@ oracle.c $(VIRGL)/vrend/virgl_video_av1_obu.c $(DAV1D_LIBS)

vt-oracle: vt-oracle.c $(VIRGL)/vrend/virgl_video_av1_obu.c
	$(CC) $(CFLAGS) -o $@ vt-oracle.c $(VIRGL)/vrend/virgl_video_av1_obu.c $(VT_LIBS)

clean:
	rm -f coverage oracle vt-oracle

.PHONY: all clean
