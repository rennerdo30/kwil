"""Rebuild NSIS bitmaps from Kwil's SVG logo (Pillow + resvg, authoring only).

Pass --renderer <svgrender.exe>, whose CLI is <in.svg> <out.png> <size>.
Keep the original artwork and website's night/ice palette.
"""
import argparse
import subprocess
import tempfile
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / 'installer/art'
OUT.mkdir(parents=True, exist_ok=True)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--renderer', required=True)
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='kwil-setup-art-') as scratch:
    rendered = Path(scratch) / 'icon.png'
    process = subprocess.Popen([
        args.renderer, str(ROOT / 'website/assets/kwil-icon.svg'), str(rendered), '512',
    ], creationflags=subprocess.CREATE_NO_WINDOW)
    print(f'SVG renderer PID {process.pid}, timeout 30 s', flush=True)
    try:
        if process.wait(timeout=30):
            raise RuntimeError('SVG rendering failed')
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
    with Image.open(rendered) as source:
        ICON = source.convert('RGBA')
FONT = Path('C:/Windows/Fonts/segoeui.ttf')

for name, size, icon_size, icon_at in [
    ('sidebar', (164, 314), 120, (22, 35)),
    ('header', (150, 57), 48, (99, 4)),
]:
    image = Image.new('RGB', size, '#0a101c')
    mark = ICON.resize((icon_size, icon_size), Image.Resampling.LANCZOS)
    image.paste(mark, icon_at, mark)
    draw = ImageDraw.Draw(image)
    if name == 'sidebar':
        draw.text((22, 176), 'Kwil', font=ImageFont.truetype(str(FONT), 30), fill='#e9f4ff')
        draw.line((22, 226, 142, 226), fill='#8fe3ff', width=2)
        for y, text in [(242, 'Plain text.'), (261, 'Full speed.')]:
            draw.text((22, y), text, font=ImageFont.truetype(str(FONT), 14), fill='#8fe3ff')
    else:
        draw.text((10, 13), 'Kwil', font=ImageFont.truetype(str(FONT), 23), fill='#e9f4ff')
    image.save(OUT / (name + '.bmp'))
