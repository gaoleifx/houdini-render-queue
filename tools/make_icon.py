"""生成应用图标 assets/app.ico（多尺寸，含 16px 简化版）。

用法：python tools/make_icon.py [--variant a|b] [--preview out.png]
配色取自 ui/app.slint 的暗色主题：canvas #101216、panel #171a1f、accent #dfa147。
"""

import sys
from PIL import Image, ImageDraw

# ── 主题色 ──────────────────────────────────────────────
BG_TOP = (28, 32, 38)
BG_BOT = (13, 15, 18)
BORDER = (59, 66, 76)
ACCENT = (223, 161, 71)
ACCENT_HI = (236, 178, 100)
BAR_MID = (78, 86, 98)
BAR_LOW = (58, 64, 73)

SS = 8  # 超采样倍数


def rounded_bg(size):
    """深色圆角方块底 + 细描边。"""
    grad = Image.new("RGB", (1, size))
    for y in range(size):
        t = y / max(1, size - 1)
        grad.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOT)))
    grad = grad.resize((size, size), Image.BICUBIC)

    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, size - 1, size - 1], radius=round(size * 0.22), fill=255
    )

    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(grad, (0, 0), mask)
    ImageDraw.Draw(out).rounded_rectangle(
        [0, 0, size - 1, size - 1],
        radius=round(size * 0.22),
        outline=BORDER + (255,),
        width=max(1, round(size * 0.013)),
    )
    return out


def variant_bars(size):
    """三条队列条目：首条琥珀色=正在跑，下面两条灰=排队。"""
    img = rounded_bg(size)
    d = ImageDraw.Draw(img)

    cw = size * 0.60
    x0 = (size - cw) / 2
    h = size * 0.145
    gap = size * 0.085
    y0 = (size - (3 * h + 2 * gap)) / 2

    rows = [  # (宽度比例, 颜色)
        (1.00, ACCENT),
        (0.84, BAR_MID),
        (0.64, BAR_LOW),
    ]
    for i, (w, col) in enumerate(rows):
        y = y0 + i * (h + gap)
        d.rounded_rectangle(
            [x0, y, x0 + cw * w, y + h], radius=h / 2, fill=col + (255,)
        )

    # 首条左端加一点高光，避免三条等长的「汉堡菜单」观感
    d.rounded_rectangle(
        [x0, y0, x0 + size * 0.055, y0 + h], radius=h / 2, fill=ACCENT_HI + (255,)
    )
    return img


def variant_play(size):
    """琥珀色播放三角 + 下方两条队列条。小尺寸只留三角，保证 16px 也认得出。"""
    img = rounded_bg(size)
    d = ImageDraw.Draw(img)

    small = size <= 24 * SS  # 16/20/24px 两倍超采样以内
    th = size * (0.50 if small else 0.44)
    tw = th * 0.88
    # 三角的视觉重心偏左，整体右移一点点才显得居中
    cx = size * (0.505 if small else 0.50)
    cy = size * (0.50 if small else 0.385)
    d.polygon(
        [
            (cx - tw / 2, cy - th / 2),
            (cx - tw / 2, cy + th / 2),
            (cx + tw / 2, cy),
        ],
        fill=ACCENT + (255,),
    )

    if small:
        return img

    # 队列条
    cw = size * 0.56
    x0 = (size - cw) / 2
    h = size * 0.105
    gap = size * 0.070
    y0 = size * 0.655
    for i, (w, col) in enumerate([(1.00, BAR_MID), (0.64, BAR_LOW)]):
        y = y0 + i * (h + gap)
        d.rounded_rectangle(
            [x0, y, x0 + cw * w, y + h], radius=h / 2, fill=col + (255,)
        )
    return img


VARIANTS = {"a": variant_bars, "b": variant_play}
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def render(fn, size):
    big = fn(size * SS).resize((size, size), Image.LANCZOS)
    return big


def main():
    args = sys.argv[1:]
    name = "a"
    preview = None
    if "--variant" in args:
        name = args[args.index("--variant") + 1]
    if "--preview" in args:
        preview = args[args.index("--preview") + 1]

    fn = VARIANTS[name]

    if preview:
        # 预览：256 / 32（放大 8 倍）/ 16（放大 16 倍），看小尺寸可读性
        p256 = render(fn, 256)
        p32 = render(fn, 32).resize((256, 256), Image.NEAREST)
        p16 = render(fn, 16).resize((256, 256), Image.NEAREST)
        canvas = Image.new("RGBA", (256 * 3 + 64, 256), (24, 26, 30, 255))
        canvas.paste(p256, (0, 0), p256)
        canvas.paste(p32, (288, 0), p32)
        canvas.paste(p16, (576, 0), p16)
        canvas.save(preview)
        print("preview:", preview)
        return

    imgs = [render(fn, s) for s in SIZES]
    imgs[-1].save(
        "assets/app.ico",
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=imgs[:-1],
    )
    # 窗口/任务栏图标（Slint 的 icon 属性走 PNG 更稳，ico 解码依赖 image crate 特性）
    imgs[-1].save("assets/app.png")
    print("written: assets/app.ico", SIZES, "+ assets/app.png")


if __name__ == "__main__":
    main()
