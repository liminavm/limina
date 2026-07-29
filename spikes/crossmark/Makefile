# crossmark: cross-API (GL / Vulkan) tier probe. See README.md for the matrix.
# Linux guest: dnf install gcc make vulkan-loader-devel glslang mesa-libEGL-devel mesa-libGLES-devel
# macOS host:  brew vulkan-loader + glslang for the vk leg; the gl leg needs a
#              mesa EGL/GLES (mesa-cs build or brew mesa) found via pkg-config
CFLAGS ?= -O2 -Wall
UNAME := $(shell uname)
ifeq ($(UNAME),Darwin)
  BREW := $(shell brew --prefix)
  CFLAGS += -I$(BREW)/include
  LDFLAGS += -L$(BREW)/lib
endif

SPV = cm.vert.spv cm_flat.frag.spv cm_tex.frag.spv

all: crossmark-vk crossmark-gl $(SPV)

crossmark-vk: crossmark_vk.c crossmark.h
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ crossmark_vk.c -lvulkan

GL_CFLAGS := $(shell pkg-config --cflags egl glesv2 2>/dev/null)
GL_LIBS := $(shell pkg-config --libs egl glesv2 2>/dev/null)
ifeq ($(GL_LIBS),)
  GL_LIBS = -lEGL -lGLESv2
endif

crossmark-gl: crossmark_gl.c crossmark.h
	$(CC) $(CFLAGS) $(GL_CFLAGS) $(LDFLAGS) -o $@ crossmark_gl.c $(GL_LIBS)

%.vert.spv: %.vert
	glslangValidator -V $< -o $@
%.frag.spv: %.frag
	glslangValidator -V $< -o $@

clean:
	rm -f crossmark-vk crossmark-gl $(SPV)
