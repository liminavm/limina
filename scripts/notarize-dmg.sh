#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Submit the outer distribution container to Apple's notary service, attach its ticket, and run
# the same Gatekeeper assessment a downloaded DMG receives. The app and every nested binary were
# already signed inside-out by build-app.sh; Apple recommends notarizing the outermost container.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

APP="${LIMINA_APP:-$ROOT/target/Limina.app}"
DMG="${1:-$ROOT/target/Limina.dmg}"
NOTARY_PROFILE="${LIMINA_NOTARY_KEYCHAIN_PROFILE:-limina-notary}"

[ -d "$APP" ] || { echo "missing app bundle: $APP" >&2; exit 1; }
[ -f "$DMG" ] || { echo "missing disk image: $DMG" >&2; exit 1; }

signature="$(codesign -dvvv "$APP" 2>&1)"
authority="$(printf '%s\n' "$signature" | sed -n 's/^Authority=//p' | head -1)"
case "$authority" in
  "Developer ID Application:"*) ;;
  *)
    echo "refusing to notarize an app not signed with Developer ID Application" >&2
    echo "resolved authority: ${authority:-<none>}" >&2
    exit 1
    ;;
esac
printf '%s\n' "$signature" | grep -q '^Timestamp=' || {
  echo "app signature has no secure timestamp" >&2
  exit 1
}

codesign --verify --deep --strict --verbose=2 "$APP"
codesign --verify --strict --verbose=2 "$DMG"

echo "==> submitting $(basename "$DMG") to Apple's notary service"
notary_result="$(mktemp)"
trap 'rm -f "$notary_result"' EXIT
if ! xcrun notarytool submit "$DMG" \
  --keychain-profile "$NOTARY_PROFILE" \
  --output-format json \
  --wait >"$notary_result"; then
  cat "$notary_result"
  exit 1
fi
cat "$notary_result"
notary_status="$(plutil -extract status raw -o - "$notary_result")"
if [ "$notary_status" != "Accepted" ]; then
  submission_id="$(plutil -extract id raw -o - "$notary_result")"
  echo "notarization finished with status '$notary_status'" >&2
  xcrun notarytool log "$submission_id" --keychain-profile "$NOTARY_PROFILE" || true
  exit 1
fi

echo "==> stapling notarization ticket"
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"

echo "==> assessing the distribution with Gatekeeper"
spctl --assess --type open --context context:primary-signature --verbose=4 "$DMG"
