# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
VIRGL := ../../third_party/virglrenderer/src
CFLAGS += -O2 -Wall -Wextra -Wno-unused-parameter -I$(VIRGL) -I$(VIRGL)/gallium/include

coverage: coverage.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f coverage

.PHONY: clean
