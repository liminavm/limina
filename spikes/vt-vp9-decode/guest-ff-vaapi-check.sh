#!/usr/bin/env bash
# Run Firefox inside a seated GNOME guest with the VA-API logs turned on, to see
# whether a clip actually reaches the hardware decoder.
#
# Copy into the guest and run it from an ssh session while the desktop is seated
# (it attaches to the session bus / Wayland socket of uid 1000):
#
#   ./guest-ff-vaapi-check.sh https://example.org/clip.webm  [log]
#
# Then read the log for the three lines that matter:
#   VA-API FFmpeg init successful            -- the decoder was created at all
#   ImportPRIMESurfaceDescriptor() FOURCC .. -- 3231564e is NV12, 30323449 is I420
#   Unsupported VA-API surface format        -- any hit means we fell back to software
#
# The FOURCC line is the whole point: ffmpeg picks the decode target by exact
# pix-fmt match, so a driver that offers I420/YV12 wins the tie over NV12 and
# Firefox then rejects the surface. See docs/hardening-backlog.md.
set -euo pipefail

url="${1:?usage: guest-ff-vaapi-check.sh <url> [logfile]}"
log="${2:-/tmp/ff-vaapi.log}"
prof="$(mktemp -d)/profile"
mkdir -p "$prof"

# Force the hardware path on rather than letting Firefox's own allowlists decide,
# so a miss is a real capability miss and not a policy one.
cat > "$prof/user.js" <<'PREFS'
user_pref("media.ffmpeg.vaapi.enabled", true);
user_pref("media.rdd-ffmpeg.enabled", true);
user_pref("media.hardware-video-decoding.force-enabled", true);
PREFS

uid="$(id -u)"
export XDG_RUNTIME_DIR="/run/user/$uid"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
export MOZ_ENABLE_WAYLAND=1
export MOZ_LOG='Dmabuf:5,PlatformDecoderModule:5,FFmpegVideo:5'
export MOZ_LOG_FILE="$log"

echo "profile: $prof"
echo "log:     $log"
firefox --profile "$prof" --new-window "$url"
