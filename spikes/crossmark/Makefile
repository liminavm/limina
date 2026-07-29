# crossmark: cross-API (GL / Vulkan) tier probe. See README.md for the matrix.
# Linux guest: dnf install gcc make vulkan-loader-devel glslang mesa-libEGL-devel \
#              mesa-libGLES-devel wayland-devel wayland-protocols-devel libwayland-egl
# macOS host:  brew vulkan-loader + glslang for the vk leg; the gl leg needs a
#              mesa EGL/GLES (mesa-cs zink-kk-prefix or brew mesa) via pkg-config.
#              No Wayland on macOS -> -p/-F compile out (offscreen only).
CFLAGS ?= -O2 -Wall
UNAME := $(shell uname)
ifeq ($(UNAME),Darwin)
  BREW := $(shell brew --prefix)
  CFLAGS += -I$(BREW)/include
  LDFLAGS += -L$(BREW)/lib
endif

SPV = cm.vert.spv cm_flat.frag.spv cm_tex.frag.spv

# -p/-F present mode: only where wayland-client exists (the Linux guest)
WL_LIBS := $(shell pkg-config --libs wayland-client 2>/dev/null)
ifneq ($(WL_LIBS),)
  CFLAGS += -DHAVE_WAYLAND $(shell pkg-config --cflags wayland-client wayland-egl)
  WL_EGL_LIBS := $(shell pkg-config --libs wayland-egl 2>/dev/null)
  XDG_XML := $(shell pkg-config --variable=pkgdatadir wayland-protocols)/stable/xdg-shell/xdg-shell.xml
  WL_SRCS = cmwin.c xdg-shell-protocol.c
  WL_HDRS = xdg-shell-client-protocol.h
endif

all: crossmark-vk crossmark-gl $(SPV)

crossmark-vk: crossmark_vk.c crossmark.h cmwin.h $(WL_HDRS) $(WL_SRCS)
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ crossmark_vk.c $(WL_SRCS) -lvulkan $(WL_LIBS)

GL_CFLAGS := $(shell pkg-config --cflags egl glesv2 2>/dev/null)
GL_LIBS := $(shell pkg-config --libs egl glesv2 2>/dev/null)
ifeq ($(GL_LIBS),)
  GL_LIBS = -lEGL -lGLESv2
endif

crossmark-gl: crossmark_gl.c crossmark.h cmwin.h $(WL_HDRS) $(WL_SRCS)
	$(CC) $(CFLAGS) $(GL_CFLAGS) $(LDFLAGS) -o $@ crossmark_gl.c $(WL_SRCS) $(GL_LIBS) $(WL_EGL_LIBS) $(WL_LIBS)

xdg-shell-client-protocol.h:
	wayland-scanner client-header $(XDG_XML) $@
xdg-shell-protocol.c:
	wayland-scanner private-code $(XDG_XML) $@

%.vert.spv: %.vert
	glslangValidator -V $< -o $@
%.frag.spv: %.frag
	glslangValidator -V $< -o $@

clean:
	rm -f crossmark-vk crossmark-gl $(SPV) xdg-shell-client-protocol.h xdg-shell-protocol.c
