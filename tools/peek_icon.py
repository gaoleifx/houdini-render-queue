"""从 exe 里取图标并渲染成 PNG，用于验证 exe 图标是否真的生效。

用法：python tools/peek_icon.py <exe> <out.png>
输出一张图：左边 32px、右边 16px（都放大 8 倍便于观察）。
"""

import ctypes
import sys
from ctypes import wintypes as wt

from PIL import Image

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
shell32 = ctypes.WinDLL("shell32", use_last_error=True)


class ICONINFO(ctypes.Structure):
    _fields_ = [
        ("fIcon", wt.BOOL),
        ("xHotspot", wt.DWORD),
        ("yHotspot", wt.DWORD),
        ("hbmMask", wt.HBITMAP),
        ("hbmColor", wt.HBITMAP),
    ]


class BITMAP(ctypes.Structure):
    _fields_ = [
        ("bmType", wt.LONG),
        ("bmWidth", wt.LONG),
        ("bmHeight", wt.LONG),
        ("bmWidthBytes", wt.LONG),
        ("bmPlanes", wt.WORD),
        ("bmBitsPixel", wt.WORD),
        ("bmBits", ctypes.c_void_p),
    ]


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wt.DWORD),
        ("biWidth", wt.LONG),
        ("biHeight", wt.LONG),
        ("biPlanes", wt.WORD),
        ("biBitCount", wt.WORD),
        ("biCompression", wt.DWORD),
        ("biSizeImage", wt.DWORD),
        ("biXPelsPerMeter", wt.LONG),
        ("biYPelsPerMeter", wt.LONG),
        ("biClrUsed", wt.DWORD),
        ("biClrImportant", wt.DWORD),
    ]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wt.DWORD * 3)]


def hicon_to_image(hicon):
    user32.GetIconInfo.argtypes = [wt.HICON, ctypes.POINTER(ICONINFO)]
    user32.GetIconInfo.restype = wt.BOOL
    gdi32.GetObjectW.argtypes = [wt.HANDLE, ctypes.c_int, ctypes.c_void_p]
    gdi32.GetObjectW.restype = ctypes.c_int
    gdi32.GetDIBits.argtypes = [
        wt.HDC,
        wt.HBITMAP,
        wt.UINT,
        wt.UINT,
        ctypes.c_void_p,
        ctypes.POINTER(BITMAPINFO),
        wt.UINT,
    ]
    gdi32.GetDIBits.restype = ctypes.c_int

    info = ICONINFO()
    if not user32.GetIconInfo(hicon, ctypes.byref(info)):
        raise OSError("GetIconInfo 失败")
    bm = BITMAP()
    gdi32.GetObjectW(info.hbmColor, ctypes.sizeof(BITMAP), ctypes.byref(bm))
    w, h = bm.bmWidth, bm.bmHeight

    bi = BITMAPINFO()
    bi.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bi.bmiHeader.biWidth = w
    bi.bmiHeader.biHeight = -h  # 负值 = 自上而下
    bi.bmiHeader.biPlanes = 1
    bi.bmiHeader.biBitCount = 32
    bi.bmiHeader.biCompression = 0  # BI_RGB

    buf = ctypes.create_string_buffer(w * h * 4)
    # hbmColor 是 DDB，GetDIBits 必须给一个 DC（NULL 只对 DIB section 有效）
    gdi32.CreateCompatibleDC.argtypes = [wt.HDC]
    gdi32.CreateCompatibleDC.restype = wt.HDC
    gdi32.DeleteDC.argtypes = [wt.HDC]
    dc = gdi32.CreateCompatibleDC(0)
    try:
        if gdi32.GetDIBits(dc, info.hbmColor, 0, h, buf, ctypes.byref(bi), 0) == 0:
            raise OSError(f"GetDIBits 失败，错误码 {ctypes.get_last_error()}")
    finally:
        gdi32.DeleteDC(dc)
    return Image.frombytes("RGBA", (w, h), bytes(buf), "raw", "BGRA")


def main():
    exe, out = sys.argv[1], sys.argv[2]
    large, small = wt.HICON(), wt.HICON()
    if shell32.ExtractIconExW(exe, 0, ctypes.byref(large), ctypes.byref(small), 1) == 0:
        raise SystemExit("exe 里没有图标")

    imgs = [hicon_to_image(large.value), hicon_to_image(small.value)]
    scaled = [im.resize((256, 256), Image.NEAREST) for im in imgs]
    canvas = Image.new("RGBA", (256 * 2 + 32, 256), (24, 26, 30, 255))
    canvas.paste(scaled[0], (0, 0), scaled[0])
    canvas.paste(scaled[1], (288, 0), scaled[1])
    canvas.save(out)
    print(f"exe 图标: large={imgs[0].size[0]}px small={imgs[1].size[0]}px -> {out}")


if __name__ == "__main__":
    main()
