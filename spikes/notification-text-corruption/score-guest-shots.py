# Score a guest screenshot: ink in the card's HEADER strip vs its BODY strip.
# Full-screen 2560x1440 guest screenshots. Card starts ~x=980,y=20.
# header text row ~y=60..85 abs; body row ~y=130..160 abs; both left of the close button.
import sys, glob
from PIL import Image
def ink(im, box):
    c = im.crop(box).convert('L')
    px = list(c.getdata())
    # card chrome is mid-grey (~85). Text is much brighter (white-ish).
    return sum(1 for p in px if p > 150)
for f in sorted(glob.glob(sys.argv[1])):
    im = Image.open(f)
    h = ink(im, (1000, 58, 1500, 88))
    b = ink(im, (1040, 130, 1560, 165))
    verdict = "NOCARD" if b < 120 else ("DAMAGED" if h < 120 else "clean")
    print(f"{f.split('/')[-1]}\thdr={h}\tbody={b}\t{verdict}")
