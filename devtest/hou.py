# -*- coding: utf-8 -*-
"""开发期用的假 hou 模块：不开 Houdini 也能验证 analyze.py / render.py 的整条链路。"""
import os
import re
import time

_current_frame = 1
HIP_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out")


class _UpdateMode:
    Manual = 0
    Auto = 1


updateMode = _UpdateMode()


def setUpdateMode(m):
    pass


def setFrame(f):
    global _current_frame
    _current_frame = int(f)


def expandString(s):
    out = str(s)
    out = out.replace("$HIP", HIP_DIR)
    out = out.replace("$JOB", HIP_DIR)
    out = out.replace("$FSTART", "1").replace("$FEND", "120")
    out = out.replace("$RFSTART", "1").replace("$RFEND", "100")
    out = re.sub(r"\$F(\d*)", lambda m: str(_current_frame).zfill(int(m.group(1) or 1)), out)
    return out


class Parm:
    def __init__(self, v):
        self.v = v

    def eval(self):
        return self.v

    def evalAsString(self):
        return str(self.v)

    def set(self, v):
        self.v = v


class NodeType:
    def __init__(self, n):
        self.n = n

    def name(self):
        return self.n


class Node:
    def __init__(self, path, type_name, parms=None, children=None):
        self._path = path
        self._type = type_name
        self._parms = parms or {}
        self._children = children or []

    def path(self):
        return self._path

    def type(self):
        return NodeType(self._type)

    def parm(self, n):
        v = self._parms.get(n)
        return Parm(v) if v is not None else None

    def children(self):
        return self._children

    def render(self, frame_range=None):
        if frame_range:
            setFrame(frame_range[0])
        out = self._parms.get("sopoutput") or self._parms.get("filename") \
            or self._parms.get("vm_picture") or self._parms.get("picture")
        if out:
            path = expandString(out)
            d = os.path.dirname(path)
            if d:
                os.makedirs(d, exist_ok=True)
            with open(path, "wb") as f:
                f.write(b"fake")
        # 单帧耗时可用环境变量放大：联调「主控重启后接管」这类时序场景时，
        # 默认 80ms 的假渲染跑得太快，块还没跑完主控就重启好了，测不到接管
        time.sleep(float(os.environ.get("HRQ_FAKE_FRAME_SEC", "0.08")))


class SopNode(Node):
    """SOP 节点：render.py 会 isinstance(node, hou.SopNode) 判断要不要关 Load From Disk"""


def _build():
    geo = Node("/out/geo_cache", "rop_geometry",
               {"sopoutput": "$HIP/geo/cache.$F4.bgeo.sc", "f1": 1, "f2": 12})
    abc = Node("/out/abc_cache", "rop_alembic",
               {"filename": "$HIP/geo/abc.$F4.abc"})
    ifd = Node("/out/mantra_beauty", "ifd",
               {"vm_picture": "$HIP/render/beauty.$F4.exr", "f1": 10, "f2": 16})
    misc = Node("/out/misc_null", "null", {})
    karma = Node("/stage/karma_render", "karma",
                 {"picture": "$HIP/render/karma.$F4.exr"})
    # /obj 下的 SOP：alembic 是导入节点（不该入列），filecache 才是要缓存的那个
    obj_geo = Node("/obj/efx_geo", "geo", {}, [
        Node("/obj/efx_geo/abc_import", "alembic",
             {"filename": "$HIP/geo/abc.$F4.abc"}),
        SopNode("/obj/efx_geo/cache_out", "filecache::2.0",
                {"sopoutput": "$HIP/geo/efx.$F4.bgeo.sc", "f1": 1, "f2": 8}),
    ])
    return {
        "/out": Node("/out", "root", {}, [geo, abc, ifd, misc]),
        "/stage": Node("/stage", "root", {}, [karma]),
        "/obj": Node("/obj", "root", {}, [obj_geo]),
    }


NODES = _build()

# 建立全路径索引（真实 hou.node() 能按任意路径取节点）
_INDEX = {}
def _index(n):
    _INDEX[n.path()] = n
    for c in n.children():
        _index(c)
for _n in NODES.values():
    _index(_n)


def node(p):
    return _INDEX.get(p)


class _Playbar:
    def frameRange(self):
        return (1, 120)


playbar = _Playbar()


class hipFile:
    @staticmethod
    def load(path, **kw):
        return True


def applicationVersionString():
    return "20.5.684-fake"
