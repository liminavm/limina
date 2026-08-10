"""Bug-1 oracle: is the half-blurred park splash a coherent mid-slide lock-curtain frame?

Tests the strip above the blur seam against two hypotheses:
  - translation: the strip is the guest wallpaper's BOTTOM rows, blurred+dimmed —
    i.e. the compositor's lock curtain (whole group translates, backdrop included)
    caught mid-slide. A legit presented frame, faithfully captured.
  - same-coords: the strip is the wallpaper at the SAME screen rows, blurred —
    i.e. a torn capture mixing the fully-covered lock frame with an older desktop frame.

Run from this directory. Needs numpy+pillow, plus wallpaper.jpg — the guest's
wallpaper (untracked; on the dogfood guest at
~/.local/share/backgrounds/2026-08-06-20-28-59-wallpaperflare.com_wallpaper(1).jpg).

Verdict 2026-08-10: seam row 939; calibration NCC 0.9994 at the exact centered
cover crop; translation NCC 0.9993 (sigma 60) vs same-coords 0.75. The splash is a
faithful capture of the curtain 43.5% down (~62ms into the 250ms EaseOutQuad slide).
"""

import numpy as np
from PIL import Image, ImageFilter

S = "."
splash = np.asarray(Image.open("./splash.png").convert("RGB")).astype(np.float64)
H, W = splash.shape[:2]

# 1. find the seam: per-row high-frequency energy (horizontal gradient magnitude)
gray = splash.mean(axis=2)
gx = np.abs(np.diff(gray, axis=1))
row_energy = gx.mean(axis=1)
# seam = the sharp jump in energy between blurred (low) and sharp (high) region
# scan rows 600..1400
best, seam = -1, None
for y in range(600, 1400):
    lo = row_energy[max(0,y-40):y].mean()
    hi = row_energy[y:y+40].mean()
    if hi - lo > best:
        best, seam = hi - lo, y
print("seam row:", seam, "energy jump:", round(best,3))

# 2. cover-scale the wallpaper to screen
wp = Image.open(f"{S}/wallpaper.jpg").convert("RGB")
ww, wh = wp.size
scale = W / ww
covH = int(round(wh * scale))
cov = wp.resize((W, covH), Image.LANCZOS)
cov = np.asarray(cov).astype(np.float64)
center_off = (covH - H) // 2
print("cover height:", covH, "center offset:", center_off)

# 3. calibrate vertical offset with the sharp bottom band, window-free x ranges
def ncc(a, b):
    a = a - a.mean(); b = b - b.mean()
    d = np.sqrt((a*a).sum() * (b*b).sum())
    return (a*b).sum()/d if d > 0 else 0.0
xs = np.r_[0:400, 3500:3840]
band = splash[2000:2150][:, xs]
best_off, best_c = None, -2
for off in range(center_off-200, center_off+201, 2):
    if off < 0 or off + H > covH: continue
    cand = cov[off+2000:off+2150][:, xs]
    c = ncc(band, cand)
    if c > best_c: best_c, best_off = c, off
print("calibrated offset:", best_off, "ncc:", round(best_c,4), "(center would be", center_off, ")")

# 4. screen-space wallpaper as displayed
disp = cov[best_off:best_off+H]
# blur+dim: radius 90 logical * 2 = 180 phys; gaussian sigma approx radius/2..3 — try a few
strip_rows = slice(250, seam-30)
actual = splash[strip_rows]
slide_rows = H - seam   # curtain bottom edge at seam => backdrop shifted up by this
for sigma in (40, 60, 90):
    blurred = np.asarray(Image.fromarray(disp.astype(np.uint8)).filter(ImageFilter.GaussianBlur(sigma))).astype(np.float64) * 0.65
    candT = blurred[strip_rows.start+slide_rows : strip_rows.stop+slide_rows]   # translation hyp
    candS = blurred[strip_rows]                                                  # same-coords hyp
    print(f"sigma {sigma}: NCC translation={ncc(actual,candT):.4f}  same-coords={ncc(actual,candS):.4f}")
