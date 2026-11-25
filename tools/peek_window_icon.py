"""检查运行中程序窗口实际使用的图标（标题栏 / 任务栏）。

用法：python tools/peek_window_icon.py <进程名关键字> <out.png>
"""

import ctypes
import sys
from ctypes import wintypes as wt

from PIL import Image

sys.path.insert(0, __file__.rsplit("\\", 1)[0] if "\\" in __file__ else ".")
from peek_icon import hicon_to_image  # noqa: E402

user32 = ctypes.WinDLL("user32", use_last_error=True)

WM_GETICON = 0x007F
ICON_SMALL, ICON_BIG = 0, 1
GCLP_HICON, GCLP_HICONSM = -14, -34

WNDENUMPROC = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


def find_windows(pid):
    found = []

    def cb(hwnd, _):
        p = wt.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(p))
        if p.value == pid and user32.IsWindowVisible(hwnd):
            n = user32.GetWindowTextLengthW(hwnd)
            buf = ctypes.create_unicode_buffer(n + 1)
            user32.GetWindowTextW(hwnd, buf, n + 1)
            found.append((hwnd, buf.value))
        return True

    user32.EnumWindows(WNDENUMPROC(cb), 0)
    return found


def main():
    needle, out = sys.argv[1], sys.argv[2]

    import csv
    import subprocess

    txt = subprocess.run(
        ["tasklist", "/FI", f"IMAGENAME eq {needle}", "/FO", "CSV", "/NH"],
        capture_output=True,
        text=True,
        encoding="gbk",
        errors="replace",
    ).stdout
    pids = [
        int(row[1])
        for row in csv.reader(txt.splitlines())
        if len(row) > 1 and row[0].lower() == needle.lower()
    ]
    if not pids:
        raise SystemExit(f"没有运行中的 {needle}")

    handles = []
    for pid in pids:
        for hwnd, title in find_windows(pid):
            for which, size in ((ICON_BIG, "big"), (ICON_SMALL, "small")):
                h = user32.SendMessageW(wt.HWND(hwnd), WM_GETICON, wt.WPARAM(which), 0)
                src = "WM_GETICON"
                if not h:
                    h = user32.GetClassLongPtrW(wt.HWND(hwnd), GCLP_HICON if which else GCLP_HICONSM)
                    src = "class"
                if h:
                    handles.append((f"{title.strip()[:20]} {size}", src, h))

    if not handles:
        raise SystemExit("窗口没有设置图标（用的是系统默认图标）")

    imgs = []
    for label, src, h in handles:
        im = hicon_to_image(h)
        imgs.append((label, src, im))
        print(f"{label}: {src} {im.size[0]}px")

    cols = len(imgs)
    canvas = Image.new("RGBA", (256 * cols + 16 * (cols + 1), 256 + 28), (24, 26, 30, 255))
    for i, (label, _src, im) in enumerate(imgs):
        s = im.resize((256, 256), Image.NEAREST)
        canvas.paste(s, (16 + i * 272, 28), s)
    canvas.save(out)
    print("->", out)


if __name__ == "__main__":
    main()
