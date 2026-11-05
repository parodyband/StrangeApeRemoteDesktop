#!/usr/bin/env python3
"""Generate sard's icons: app icon (all sizes), tray/menu-bar icons, and raw
RGBA tray assets embedded in the binary. Pure Pillow, drawn at 4x and
downsampled for anti-aliasing.

    python3 tools/icon/make_icons.py
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parents[2]
APP = ROOT / "assets/icons/app"
TRAY = ROOT / "assets/icons/tray"
EMBED = ROOT / "crates/sard/assets/tray"
SS = 4  # supersampling factor

# Palette
BG_TOP = (124, 92, 255)
BG_BOTTOM = (38, 22, 104)
FUR = (43, 30, 58)
FUR_HI = (66, 48, 86)
SKIN = (244, 203, 164)
SKIN_SHADE = (226, 178, 138)
EYE = (24, 16, 32)
BEZEL = (240, 238, 250)
SCREEN_A = (32, 214, 214)
SCREEN_B = (58, 82, 236)


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(len(a)))


def gradient(size, c0, c1, diagonal=True):
    w, h = size
    g = Image.new("RGB", (w, h))
    px = g.load()
    for y in range(h):
        for x in range(w):
            t = ((x / w) * 0.35 + (y / h) * 0.65) if diagonal else y / h
            px[x, y] = lerp(c0, c1, t)
    return g


def squircle_mask(size, radius_frac=0.225):
    w, h = size
    m = Image.new("L", size, 0)
    ImageDraw.Draw(m).rounded_rectangle((0, 0, w - 1, h - 1), radius=int(w * radius_frac), fill=255)
    return m


def ellipse(d, cx, cy, rx, ry, fill):
    d.ellipse((cx - rx, cy - ry, cx + rx, cy + ry), fill=fill)


def app_icon(px=1024):
    S = px * SS
    u = S / 1024.0  # design units are a 1024 grid

    def U(v):
        return int(v * u)

    # Background squircle with a soft vertical gradient (macOS grid: 100 px margin).
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    inset = U(100)
    body = (S - 2 * inset, S - 2 * inset)
    bg = gradient((body[0] // 8, body[1] // 8), BG_TOP, BG_BOTTOM).resize(body, Image.BICUBIC)
    img.paste(bg, (inset, inset), squircle_mask(body))

    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    cx = U(512)

    # Ape head (behind the screen)
    ellipse(d, cx - U(205), U(392), U(68), U(68), FUR)          # ears
    ellipse(d, cx + U(205), U(392), U(68), U(68), FUR)
    ellipse(d, cx - U(205), U(392), U(40), U(40), SKIN_SHADE)
    ellipse(d, cx + U(205), U(392), U(40), U(40), SKIN_SHADE)
    ellipse(d, cx, U(388), U(200), U(196), FUR)                  # head
    ellipse(d, cx - U(28), U(300), U(120), U(70), FUR_HI)        # soft highlight on the crown
    ellipse(d, cx, U(388), U(200), U(196), None)
    # Face: heart shape = two upper lobes + muzzle
    ellipse(d, cx - U(66), U(372), U(86), U(82), SKIN)
    ellipse(d, cx + U(66), U(372), U(86), U(82), SKIN)
    ellipse(d, cx, U(462), U(128), U(86), SKIN)
    ellipse(d, cx, U(480), U(104), U(62), SKIN_SHADE)            # lower muzzle shade
    ellipse(d, cx, U(470), U(110), U(62), SKIN)
    # Eyes
    for s in (-1, 1):
        ellipse(d, cx + s * U(62), U(372), U(26), U(28), EYE)
        ellipse(d, cx + s * U(62) + U(9), U(362), U(8), U(8), (255, 255, 255))
    # Nostrils + smile
    ellipse(d, cx - U(20), U(438), U(10), U(8), EYE)
    ellipse(d, cx + U(20), U(438), U(10), U(8), EYE)
    d.arc((cx - U(52), U(440), cx + U(52), U(500)), start=20, end=160, fill=EYE, width=U(10))

    # Monitor: bezel, screen with "stream" gradient, stand
    top = U(556)
    d.rounded_rectangle((U(158), top, U(866), U(868)), radius=U(46), fill=BEZEL)
    scr_box = (U(190), top + U(32), U(834), U(836))
    scr = gradient((64, 64), SCREEN_A, SCREEN_B).resize((scr_box[2] - scr_box[0], scr_box[3] - scr_box[1]), Image.BICUBIC)
    m = Image.new("L", scr.size, 0)
    ImageDraw.Draw(m).rounded_rectangle((0, 0, scr.size[0] - 1, scr.size[1] - 1), radius=U(26), fill=255)
    layer.paste(scr, scr_box[:2], m)
    # Screen glare + "signal" chevrons (remote stream)
    glare = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glare)
    gd.polygon([(U(190), top + U(32)), (U(470), top + U(32)), (U(330), U(836)), (U(190), U(836))], fill=(255, 255, 255, 34))
    layer = Image.alpha_composite(layer, glare)
    d = ImageDraw.Draw(layer)
    for i, r in enumerate((U(70), U(118), U(166))):
        d.arc((cx - r, U(806) - r, cx + r, U(806) + r), start=225, end=315, fill=(255, 255, 255, 235 - 40 * i), width=U(22))
    ellipse(d, cx, U(806), U(20), U(20), (255, 255, 255, 240))
    d.polygon([(U(452), U(868)), (U(572), U(868)), (U(596), U(912)), (U(428), U(912))], fill=BEZEL)

    # Hands gripping the top of the bezel (in front)
    for s in (-1, 1):
        hx = cx + s * U(196)
        ellipse(d, hx, top - U(6), U(78), U(46), FUR)
        for f in range(4):
            fx = hx + (f - 1.5) * U(30)
            ellipse(d, fx, top + U(20), U(17), U(24), SKIN)

    img = Image.alpha_composite(img, layer)
    # Subtle drop shadow under the whole tile.
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle((inset, inset + U(14), S - inset, S - inset + U(14)), radius=int(body[0] * 0.225), fill=(0, 0, 0, 90))
    shadow = shadow.filter(ImageFilter.GaussianBlur(U(18)))
    out = Image.alpha_composite(shadow, img)
    return out.resize((px, px), Image.LANCZOS)


def tray_mono(px, active):
    """Menu-bar template (black + alpha): ape face cut-out above a screen.
    Idle = screen outline, active (connected / being viewed) = filled screen."""
    S = px * SS
    u = S / 22.0
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    black, clear = (0, 0, 0, 255), (0, 0, 0, 0)

    def U(v):
        return v * u

    # Head + ears (solid)
    ellipse(d, U(4.4), U(6.9), U(2.1), U(2.1), black)
    ellipse(d, U(17.6), U(6.9), U(2.1), U(2.1), black)
    ellipse(d, U(11), U(6.6), U(6.0), U(5.9), black)
    # Heart-shaped face punched out
    ellipse(d, U(9.0), U(6.2), U(2.5), U(2.3), clear)
    ellipse(d, U(13.0), U(6.2), U(2.5), U(2.3), clear)
    ellipse(d, U(11), U(8.9), U(3.9), U(2.6), clear)
    # Eyes back in
    ellipse(d, U(9.1), U(6.1), U(0.95), U(1.05), black)
    ellipse(d, U(12.9), U(6.1), U(0.95), U(1.05), black)
    # Screen below
    box = (U(2.0), U(13.4), U(20.0), U(20.6))
    d.rounded_rectangle(box, radius=U(1.6), fill=black)
    if not active:
        d.rounded_rectangle((U(3.7), U(15.1), U(18.3), U(18.9)), radius=U(0.7), fill=clear)
    return img.resize((px, px), Image.LANCZOS)


def tray_color(px, active):
    icon = app_icon(256)
    # Crop the tile margin away so the art fills the tray slot.
    m = int(256 * 100 / 1024)
    icon = icon.crop((m, m, 256 - m, 256 - m)).resize((px, px), Image.LANCZOS)
    if active:
        d = ImageDraw.Draw(icon)
        r = px * 0.17
        cx, cy = px - r - px * 0.02, px - r - px * 0.02
        d.ellipse((cx - r - 1, cy - r - 1, cx + r + 1, cy + r + 1), fill=(255, 255, 255, 255))
        d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=(46, 204, 113, 255))
    return icon


def main():
    APP.mkdir(parents=True, exist_ok=True)
    TRAY.mkdir(parents=True, exist_ok=True)
    EMBED.mkdir(parents=True, exist_ok=True)
    big = app_icon(1024)
    for s in (16, 32, 48, 64, 128, 256, 512, 1024):
        (big if s == 1024 else big.resize((s, s), Image.LANCZOS)).save(APP / f"sard-{s}.png")
    for s in (22, 44):
        tray_mono(s, False).save(TRAY / f"mono-{s}.png")
        tray_mono(s, True).save(TRAY / f"mono-active-{s}.png")
    for s in (22, 44, 64):
        tray_color(s, False).save(TRAY / f"color-{s}.png")
        tray_color(s, True).save(TRAY / f"color-active-{s}.png")
    # Raw RGBA for embedding (no PNG decoder in the binary).
    for name, img in (
        ("mono.rgba", tray_mono(44, False)),
        ("mono_active.rgba", tray_mono(44, True)),
        ("color.rgba", tray_color(64, False)),
        ("color_active.rgba", tray_color(64, True)),
    ):
        (EMBED / name).write_bytes(img.convert("RGBA").tobytes())
    print("icons written")


if __name__ == "__main__":
    main()
