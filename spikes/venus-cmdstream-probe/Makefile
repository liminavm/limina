# drawstorm: native Vulkan command-stream probe (guest venus / host KK A/B).
# Linux guest: dnf install gcc vulkan-loader-devel glslang
# macOS host:  brew vulkan-loader + glslang; VK_ICD_FILENAMES -> KK devenv json
CFLAGS ?= -O2 -Wall
LDLIBS = -lvulkan
UNAME := $(shell uname)
ifeq ($(UNAME),Darwin)
  BREW := $(shell brew --prefix)
  CFLAGS += -I$(BREW)/include
  LDFLAGS += -L$(BREW)/lib
endif

all: drawstorm vert.spv frag.spv

drawstorm: drawstorm.c
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $< $(LDLIBS)

vert.spv: shader.vert
	glslangValidator -V $< -o $@
frag.spv: shader.frag
	glslangValidator -V $< -o $@

clean:
	rm -f drawstorm vert.spv frag.spv
