#!/usr/bin/env python3
"""Draws the lgdscan icon: a picture with a translucent logo on it, the
corners of a selection round the logo, and a scan line through it, past
which the logo stands alone on a checkerboard (taken out of the picture).

Writes icon.svg, icon-<size>.png and icon.ico (for the Windows programs)
beside this script (needs pycairo and Pillow)."""
import math
import os
import cairo

HERE = os.path.dirname(os.path.abspath(__file__))


def rounded(cr, x, y, w, h, r):
    cr.new_sub_path()
    cr.arc(x + w - r, y + r, r, -math.pi / 2, 0)
    cr.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
    cr.arc(x + r, y + h - r, r, math.pi / 2, math.pi)
    cr.arc(x + r, y + r, r, math.pi, 3 * math.pi / 2)
    cr.close_path()


def draw(cr):
    # The screen.
    rounded(cr, 16, 16, 224, 224, 44)
    g = cairo.LinearGradient(0, 16, 0, 240)
    g.add_color_stop_rgb(0, 0x2c / 255, 0x41 / 255, 0x70 / 255)
    g.add_color_stop_rgb(1, 0x10 / 255, 0x17 / 255, 0x2a / 255)
    cr.set_source(g)
    cr.fill_preserve()
    cr.save()
    cr.clip()
    # A picture on it: two soft hills.
    for (y0, y1, c0, c1) in ((176, 150, (0x2a, 0x8c, 0x96), (0x17, 0x4e, 0x63)), (206, 188, (0x1d, 0x63, 0x73), (0x0f, 0x2c, 0x3c))):
        cr.move_to(16, y0)
        cr.curve_to(90, y0 - 44, 150, y1 + 30, 240, y1)
        cr.line_to(240, 240)
        cr.line_to(16, 240)
        cr.close_path()
        g = cairo.LinearGradient(0, y1 - 30, 0, 240)
        g.add_color_stop_rgb(0, *(v / 255 for v in c0))
        g.add_color_stop_rgb(1, *(v / 255 for v in c1))
        cr.set_source(g)
        cr.fill()
    cr.restore()

    bx, by, bw, bh = 100, 54, 110, 58
    x0, y0, x1, y1 = bx - 16, by - 16, bx + bw + 16, by + bh + 16
    split = bx + bw * 0.5

    # Right of the scan line, the logo taken out of the picture: what is
    # behind it is gone, shown as a checkerboard.
    cr.save()
    cr.rectangle(split, y0, x1 - split, y1 - y0)
    cr.clip()
    rounded(cr, x0, y0, x1 - x0, y1 - y0, 10)
    cr.set_source_rgb(0x47 / 255, 0x50 / 255, 0x66 / 255)
    cr.fill()
    cell = 11
    cr.set_source_rgb(0x2f / 255, 0x36 / 255, 0x48 / 255)
    for i in range(int((x1 - split) / cell) + 1):
        for j in range(int((y1 - y0) / cell) + 1):
            if (i + j) % 2:
                cr.rectangle(split + i * cell, y0 + j * cell, cell, cell)
    cr.fill()
    cr.restore()

    # The logo: a translucent plate with an emblem and a bar of lettering
    # cut out of it.
    cr.push_group()
    rounded(cr, bx, by, bw, bh, 14)
    cr.set_source_rgba(1, 1, 1, 0.66)
    cr.fill()
    cr.set_operator(cairo.OPERATOR_CLEAR)
    # A four-pointed star: each side curves in toward the middle.
    ex, ey, r, k = bx + 31, by + bh / 2, 19, 5
    tips = ((0, -1), (1, 0), (0, 1), (-1, 0))
    cr.move_to(ex, ey - r)
    for n in range(4):
        (ax, ay), (bx2, by2) = tips[n], tips[(n + 1) % 4]
        c = (ex + (ax + bx2) * k, ey + (ay + by2) * k)
        cr.curve_to(*c, *c, ex + bx2 * r, ey + by2 * r)
    cr.close_path()
    cr.fill()
    rounded(cr, bx + 58, by + 17, bw - 58 - 16, 10, 5)
    cr.fill()
    rounded(cr, bx + 58, by + bh - 27, bw - 58 - 30, 10, 5)
    cr.fill()
    cr.pop_group_to_source()
    cr.paint()

    # The corners of the selection round it.
    cr.set_source_rgb(0xff / 255, 0xb5 / 255, 0x47 / 255)
    cr.set_line_width(9)
    cr.set_line_cap(cairo.LINE_CAP_ROUND)
    cr.set_line_join(cairo.LINE_JOIN_ROUND)
    arm = 22
    for (x, y, dx, dy) in ((x0, y0, 1, 1), (x1, y0, -1, 1), (x0, y1, 1, -1), (x1, y1, -1, -1)):
        cr.move_to(x + dx * arm, y)
        cr.line_to(x, y)
        cr.line_to(x, y + dy * arm)
        cr.stroke()

    # The scan line, glowing.
    for w, a in ((14, 0.14), (8, 0.28), (3.5, 1.0)):
        cr.set_source_rgba(0xff / 255, 0xd5 / 255, 0x8a / 255, a)
        cr.set_line_width(w)
        cr.move_to(split, y0 - 8)
        cr.line_to(split, y1 + 8)
        cr.stroke()


def main():
    svg = cairo.SVGSurface(os.path.join(HERE, "icon.svg"), 256, 256)
    draw(cairo.Context(svg))
    svg.finish()
    for size in (16, 24, 32, 48, 64, 128, 256, 512):
        s = cairo.ImageSurface(cairo.FORMAT_ARGB32, size, size)
        cr = cairo.Context(s)
        cr.scale(size / 256, size / 256)
        draw(cr)
        s.write_to_png(os.path.join(HERE, f"icon-{size}.png"))
    from PIL import Image

    sizes = [16, 24, 32, 48, 64, 256]
    images = [Image.open(os.path.join(HERE, f"icon-{n}.png")) for n in sizes]
    images[-1].save(os.path.join(HERE, "icon.ico"), sizes=[(n, n) for n in sizes], append_images=images[:-1])


if __name__ == "__main__":
    main()
