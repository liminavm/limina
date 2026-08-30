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

coverage: coverage.c
	$(CC) $(CFLAGS) -o $@ $<

oracle: oracle.c $(VIRGL)/vrend/virgl_video_av1_obu.c
	$(CC) $(CFLAGS) $(DAV1D_CFLAGS) -o $@ oracle.c $(VIRGL)/vrend/virgl_video_av1_obu.c $(DAV1D_LIBS)

clean:
	rm -f coverage oracle

.PHONY: all clean
