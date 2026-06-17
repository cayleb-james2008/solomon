"""Generate the Solomon app icon — a clay rounded-square tile with the infinity (∞)
brand mark, rendered crisply from a system font glyph (not pixel art). Outputs a
multi-size Windows .ico (embedded into the exe by solomon.spec) plus a 256px PNG.

Run:  uv run --with pillow python web/assets/make_icon.py
"""
import os
from PIL import Image, ImageDraw, ImageFont

SIZE = 512
MARGIN = 26
RADIUS = 116
CLAY = (217, 119, 87, 255)      # #d97757 — Solomon's clay accent
CREAM = (250, 249, 245, 255)    # #faf9f5 — high-contrast mark
GLYPH = "∞"                # ∞
HERE = os.path.dirname(os.path.abspath(__file__))

img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
d.rounded_rectangle([MARGIN, MARGIN, SIZE - MARGIN, SIZE - MARGIN], radius=RADIUS, fill=CLAY)

font_path = next((p for p in (
    r"C:\Windows\Fonts\arialbd.ttf", r"C:\Windows\Fonts\segoeuib.ttf",
    r"C:\Windows\Fonts\arial.ttf", r"C:\Windows\Fonts\seguisym.ttf",
) if os.path.exists(p)), None)

inner = SIZE - 2 * MARGIN
if font_path:
    # auto-fit: largest size where the glyph stays within ~66% width / 56% height of the tile
    chosen = None
    for s in range(60, 480, 4):
        f = ImageFont.truetype(font_path, s)
        bb = d.textbbox((0, 0), GLYPH, font=f)
        if (bb[2] - bb[0]) <= inner * 0.66 and (bb[3] - bb[1]) <= inner * 0.56:
            chosen = (f, bb)
        else:
            break
    f, bb = chosen
    gw, gh = bb[2] - bb[0], bb[3] - bb[1]
    d.text(((SIZE - gw) / 2 - bb[0], (SIZE - gh) / 2 - bb[1]), GLYPH, font=f, fill=CREAM)
else:
    # fallback: two thick rings forming an ∞ (still smooth, anti-aliased on downscale)
    r, w, cy = 86, 34, SIZE // 2
    for cx in (SIZE // 2 - r + w, SIZE // 2 + r - w):
        d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=CREAM, width=w)

ico = os.path.join(HERE, "icon.ico")
img.save(ico, format="ICO",
         sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
img.resize((256, 256), Image.LANCZOS).save(os.path.join(HERE, "icon.png"))
print("wrote", ico, "and icon.png  (font:", font_path or "fallback rings", ")")
