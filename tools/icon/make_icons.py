#!/usr/bin/env python3
"""Generate sard's icons from assets/icons/sard.svg: app icon (all sizes),
tray/menu-bar icons, and raw RGBA tray assets embedded in the binary.

Needs Pillow plus the librsvg and cairo shared libraries (loaded with ctypes;
present on most Linux desktops, `brew install librsvg` on macOS).

    python3 tools/icon/make_icons.py
"""
import ctypes
import ctypes.util
import re
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
SVG = ROOT / "assets/icons/sard.svg"
APP = ROOT / "assets/icons/app"
TRAY = ROOT / "assets/icons/tray"
EMBED = ROOT / "crates/sard/assets/tray"
SS = 4  # supersampling factor for the hand-drawn mono icons


def load_lib(names):
    for name in names:
        for candidate in (name, ctypes.util.find_library(name.split(".")[0].removeprefix("lib"))):
            if not candidate:
                continue
            try:
                return ctypes.CDLL(candidate)
            except OSError:
                pass
    raise SystemExit(f"could not load any of {names}")


class RsvgRectangle(ctypes.Structure):
    _fields_ = [(n, ctypes.c_double) for n in ("x", "y", "width", "height")]


rsvg = load_lib(["librsvg-2.so.2", "librsvg-2.2.dylib", "/opt/homebrew/lib/librsvg-2.2.dylib", "/usr/local/lib/librsvg-2.2.dylib"])
cairo = load_lib(["libcairo.so.2", "libcairo.2.dylib", "/opt/homebrew/lib/libcairo.2.dylib", "/usr/local/lib/libcairo.2.dylib"])
vp = ctypes.c_void_p
rsvg.rsvg_handle_new_from_data.restype = vp
rsvg.rsvg_handle_new_from_data.argtypes = [ctypes.c_char_p, ctypes.c_size_t, vp]
rsvg.rsvg_handle_render_document.restype = ctypes.c_int
rsvg.rsvg_handle_render_document.argtypes = [vp, vp, ctypes.POINTER(RsvgRectangle), vp]
cairo.cairo_image_surface_create.restype = vp
cairo.cairo_image_surface_create.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int]
cairo.cairo_image_surface_get_data.restype = ctypes.POINTER(ctypes.c_ubyte)
cairo.cairo_image_surface_get_data.argtypes = [vp]
cairo.cairo_image_surface_get_stride.restype = ctypes.c_int
cairo.cairo_image_surface_get_stride.argtypes = [vp]
cairo.cairo_create.restype = vp
cairo.cairo_create.argtypes = [vp]
for fn in ("cairo_surface_flush", "cairo_surface_destroy", "cairo_destroy"):
    getattr(cairo, fn).argtypes = [vp]
gobject = load_lib(["libgobject-2.0.so.0", "libgobject-2.0.0.dylib", "/opt/homebrew/lib/libgobject-2.0.0.dylib", "/usr/local/lib/libgobject-2.0.0.dylib"])
gobject.g_object_unref.argtypes = [vp]


def render_svg(svg: str, px: int) -> Image.Image:
    data = svg.encode()
    handle = rsvg.rsvg_handle_new_from_data(data, len(data), None)
    if not handle:
        raise SystemExit("librsvg could not parse the SVG")
    surf = cairo.cairo_image_surface_create(0, px, px)  # CAIRO_FORMAT_ARGB32
    cr = cairo.cairo_create(surf)
    if not rsvg.rsvg_handle_render_document(handle, cr, ctypes.byref(RsvgRectangle(0, 0, px, px)), None):
        raise SystemExit("librsvg could not render the SVG")
    cairo.cairo_surface_flush(surf)
    stride = cairo.cairo_image_surface_get_stride(surf)
    buf = ctypes.string_at(cairo.cairo_image_surface_get_data(surf), stride * px)
    # Cairo stores premultiplied native-endian ARGB (BGRA in memory on little-endian).
    img = Image.frombuffer("RGBA", (px, px), buf, "raw", "BGRa", stride, 1).copy()
    cairo.cairo_destroy(cr)
    cairo.cairo_surface_destroy(surf)
    gobject.g_object_unref(handle)
    return img


# The logo is a monitor on a squircle tile. The tile is drawn as an outline
# only (fill="none"), so it is the first unfilled path.
TILE_RE = re.compile(r'<path\b[^>]*\bfill="none"[^>]*/>')


def svg_source():
    svg = SVG.read_text()
    if not TILE_RE.search(svg):
        raise SystemExit(f"{SVG}: no tile outline path (fill=\"none\") found")
    return svg


def app_svg():
    """Tile filled white, so the icon is opaque on dark docks and taskbars."""
    return TILE_RE.sub(lambda m: m.group(0).replace('fill="none"', 'fill="#ffffff"', 1), svg_source(), count=1)


def art_svg():
    """Just the monitor (no tile), for tray slots where every pixel counts."""
    return TILE_RE.sub("", svg_source(), count=1)


def app_icon(px=1024):
    return render_svg(app_svg(), px)


def tray_color(px, active):
    big = render_svg(art_svg(), 1024)
    l, t, r, b = big.getchannel("A").getbbox()
    # Square crop around the artwork with a small margin.
    side = int(max(r - l, b - t) * 1.04)
    cx, cy = (l + r) // 2, (t + b) // 2
    icon = big.crop((cx - side // 2, cy - side // 2, cx - side // 2 + side, cy - side // 2 + side))
    icon = icon.resize((px, px), Image.LANCZOS)
    if active:
        d = ImageDraw.Draw(icon)
        rad = px * 0.17
        x, y = px - rad - px * 0.02, px - rad - px * 0.02
        d.ellipse((x - rad - 1, y - rad - 1, x + rad + 1, y + rad + 1), fill=(255, 255, 255, 255))
        d.ellipse((x - rad, y - rad, x + rad, y + rad), fill=(46, 204, 113, 255))
    return icon


def tray_mono(px, active):
    """Menu-bar template (black + alpha) of the logo: a monitor with two
    "eyes" in the top of the bezel, a stacked back layer peeking out at the
    left and bottom, and a stand. Idle = hollow screen, active (connected /
    being viewed) = filled screen."""
    S = px * SS
    u = S / 22.0
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    black, clear = (0, 0, 0, 255), (0, 0, 0, 0)

    def U(*v):
        return [x * u for x in v]

    front = U(3.6, 1.8, 20.6, 14.4)
    # Stand (behind the monitor) and base.
    d.polygon([tuple(U(8.9, 14.0)), tuple(U(13.9, 14.0)), tuple(U(13.4, 17.6)), tuple(U(8.0, 17.6))], fill=black)
    d.rounded_rectangle(U(5.6, 17.4, 16.4, 19.8), radius=0.8 * u, fill=black)
    # Back layer, offset left/down, separated from the front by a clear gap.
    d.rounded_rectangle(U(1.4, 3.2, 18.4, 15.8), radius=3.0 * u, fill=black)
    d.rounded_rectangle(U(2.8, 1.1, 21.3, 15.1), radius=3.6 * u, fill=clear)
    # Front bezel.
    d.rounded_rectangle(front, radius=3.0 * u, fill=black)
    # Eyes punched out of the top of the bezel.
    for ex in (6.4, 8.7):
        d.ellipse(U(ex - 0.95, 3.0, ex + 0.95, 4.9), fill=clear)
    # Screen.
    screen = U(5.6, 5.9, 18.6, 12.4)
    d.rounded_rectangle(screen, radius=1.0 * u, fill=clear)
    if active:
        d.rounded_rectangle(U(6.9, 7.2, 17.3, 11.1), radius=0.4 * u, fill=black)
    return img.resize((px, px), Image.LANCZOS)


def main():
    APP.mkdir(parents=True, exist_ok=True)
    TRAY.mkdir(parents=True, exist_ok=True)
    EMBED.mkdir(parents=True, exist_ok=True)
    big = app_icon(1024)
    for s in (16, 32, 48, 64, 128, 256, 512, 1024):
        (big if s == 1024 else big.resize((s, s), Image.LANCZOS)).save(APP / f"sard-{s}.png", optimize=True)
    for s in (22, 44):
        tray_mono(s, False).save(TRAY / f"mono-{s}.png", optimize=True)
        tray_mono(s, True).save(TRAY / f"mono-active-{s}.png", optimize=True)
    for s in (22, 44, 64):
        tray_color(s, False).save(TRAY / f"color-{s}.png", optimize=True)
        tray_color(s, True).save(TRAY / f"color-active-{s}.png", optimize=True)
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
