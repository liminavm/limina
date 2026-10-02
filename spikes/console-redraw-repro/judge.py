# Is the passphrase prompt on glass at the end of a boot? Compares the last presented frame's
# prompt row (minus the cursor) with the reference from a healthy boot.
import sys,glob,os
from PIL import Image, ImageChops
D=os.environ.get('CONREDRAW_DIR',os.path.join(os.path.dirname(os.path.abspath(__file__)),'work.noindex'))
ref=Image.open(sorted(glob.glob(D+'/ref/frames/*.png'))[-1]).convert('L').crop((0,0,470,70))
I=sys.argv[1]
prompt='Please unlock disk' in open(I+'/console.log',errors='replace').read()
frames=sorted(glob.glob(I+'/frames/*.png'))
if not frames: print('NOFRAMES', prompt); sys.exit()
im=Image.open(frames[-1]).convert('L')
if im.size!=(2560,1440): print('STUCK size', im.size, frames[-1].split('/')[-1], 'prompt-on-serial', prompt); sys.exit()
diff=ImageChops.difference(im.crop((0,0,470,70)),ref)
d=sum(diff.get_flattened_data())/(470*70)
print(('OK' if d<2 else 'STUCK'), f'd={d:.1f}', frames[-1].split('/')[-1], len(frames),'frames prompt-on-serial', prompt)
