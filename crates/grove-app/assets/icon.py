"""Generates Grove's icon: the layered Grove.icon (macOS 26+ Liquid Glass),
a flat preview, and the menu bar template.

    python3 crates/grove-app/assets/icon.py

The bitmaps the app embeds are rendered from the SVGs (any SVG rasterizer;
e.g. resvg + ImageMagick):

    dock-icon.png          icon-preview.svg at 824px, padded to 1024x1024
                           (the Dock icon when running outside Grove.app)
    tray-template.rgba     tray.svg trimmed, fitted into 36x36, raw RGBA
                           (magick tray.png -trim +repage -resize 34x34
                            -background none -gravity center -extent 36x36
                            -depth 8 rgba:tray-template.rgba)

scripts/bundle.sh compiles Grove.icon with Xcode's actool.
"""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
SIZE = 1024
DY = 52  # centres the group vertically

FRONT = ["#ffffff", "#c4f3d9", "#8fe3b6"]
BACK = ["#7fd3a6", "#5fbf8c", "#46a877"]
BG = ("#17402f", "#07170f")


def tier(cx, apex, w, h, fill, r):
    l, rgt, b = cx - w / 2, cx + w / 2, apex + h
    return (f'<path d="M{cx:.1f} {apex:.1f} L{rgt:.1f} {b:.1f} L{l:.1f} {b:.1f} Z" fill="{fill}" '
            f'stroke="{fill}" stroke-width="{2 * r:.1f}" stroke-linejoin="round"/>')


def pine(cx, top, s, fills, trunk):
    """Three stacked tiers and a trunk; upper tiers drawn over lower ones."""
    r = 15 * s
    tiers = [tier(cx, top + i * 140 * s, (320 + 112 * i) * s, (204 + 42 * i) * s, f, r)
             for i, f in enumerate(fills)]
    base = top + 2 * 140 * s + 288 * s
    tw, th = 52 * s, 120 * s
    stem = (f'<rect x="{cx - tw / 2:.1f}" y="{base - 20 * s:.1f}" width="{tw:.1f}" '
            f'height="{th:.1f}" rx="{tw / 2:.1f}" fill="{trunk}"/>')
    return stem + "".join(reversed(tiers))


def back_pines(fills=BACK, trunk=BACK[2]):
    return pine(290, 330 + DY, 0.66, fills, trunk) + pine(734, 330 + DY, 0.66, fills, trunk)


def front_pine(fills=FRONT, trunk=FRONT[2]):
    return pine(512, 170 + DY, 0.9, fills, trunk)


def svg(body, defs=""):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" '
            f'viewBox="0 0 {SIZE} {SIZE}"><defs>{defs}</defs>{body}</svg>\n')


def write(path, text):
    with open(os.path.join(HERE, path), "w") as f:
        f.write(text)


def srgb(hex_color):
    r, g, b = (int(hex_color[i:i + 2], 16) / 255 for i in (1, 3, 5))
    return f"srgb:{r:.5f},{g:.5f},{b:.5f},1.00000"


# Layered icon for Icon Composer / actool.
write("Grove.icon/Assets/back.svg", svg(back_pines()))
write("Grove.icon/Assets/front.svg", svg(front_pine()))
icon = {
    "fill": {"linear-gradient": [srgb(BG[0]), srgb(BG[1])]},
    "groups": [
        {
            "name": "Front",
            "layers": [{"name": "front", "image-name": "front.svg", "fill": "automatic",
                        "glass": True, "hidden": False, "blend-mode": "normal"}],
            "lighting": "individual",
            "shadow": {"kind": "neutral", "opacity": 0.5},
            "translucency": {"enabled": False, "value": 0.5},
        },
        {
            "name": "Back",
            "layers": [{"name": "back", "image-name": "back.svg", "fill": "automatic",
                        "glass": True, "hidden": False, "blend-mode": "normal"}],
            "lighting": "individual",
            "shadow": {"kind": "neutral", "opacity": 0.4},
            "translucency": {"enabled": True, "value": 0.35},
        },
    ],
    "supported-platforms": {"squares": "shared"},
}
write("Grove.icon/icon.json", json.dumps(icon, indent=2) + "\n")

# Flat preview (also the source of the README image).
bg = (f'<linearGradient id="bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{BG[0]}"/>'
      f'<stop offset="1" stop-color="{BG[1]}"/></linearGradient>'
      '<radialGradient id="shine" cx="0.3" cy="0.15" r="0.8"><stop offset="0" stop-color="#fff" '
      'stop-opacity="0.2"/><stop offset="1" stop-color="#fff" stop-opacity="0"/></radialGradient>'
      '<clipPath id="sq"><rect width="1024" height="1024" rx="229" ry="229"/></clipPath>')
write("icon-preview.svg", svg(
    '<g clip-path="url(#sq)"><rect width="1024" height="1024" fill="url(#bg)"/>'
    '<rect width="1024" height="1024" fill="url(#shine)"/>' + back_pines() + front_pine() + "</g>", bg))

# Menu bar template: black silhouette, with a gap cut around the front pine
# so the three trees stay readable at 18pt.
black = ["#000"] * 3
gap = front_pine(["#000"] * 3, "#000").replace('stroke-width="27.0"', 'stroke-width="110"')
write("tray.svg", svg(
    f'<g mask="url(#cut)">{back_pines(black, "#000")}</g>{front_pine(black, "#000")}',
    f'<mask id="cut"><rect width="1024" height="1024" fill="#fff"/>'
    f'<g>{gap.replace("#000", "#000")}</g></mask>'))
