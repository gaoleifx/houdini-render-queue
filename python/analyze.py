#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
analyze.py — 在 hython 中加载 .hip，枚举可输出的缓存/渲染节点，输出 JSON。

用法:
    hython analyze.py <hip_path> [--out <json_path>]

输出 JSON 结构:
{
  "hip": "D:/shots/sh010/efx.hip",
  "houdini": "20.5.684",
  "error": null,
  "global": {"fstart": 1, "fend": 120, "rfstart": 1, "rfend": 120},
  "nodes": [
    {"path": "/out/geo_cache", "type": "rop_geometry", "category": "cache",
     "output": "$HIP/geo/sh010.$F4.bgeo.sc", "fstart": 1, "fend": 120,
     "frame_source": "node"}
  ]
}
"""
import sys
import json
import argparse


# 渲染类节点类型名（小写比较）
RENDER_TYPES = {
    "ifd", "mantra", "karma", "redshift_rop", "arnold", "octane",
    "renderman", "vray_renderer", "husk", "usdrender", "usd_render_rop",
    "rop_usd_render", "rop_karma", "rop_redshift", "rop_arnold",
    # Houdini 20.5+ 的 USDRender ROP：真正跑 husk 渲染的那个节点
    "usdrender_rop", "usdrenderrop",
}
# 缓存类节点类型名
CACHE_TYPES = {
    "rop_geometry", "alembic", "rop_alembic", "filecache", "filecache::2.0",
    "rop_usd", "usd_rop", "rop_fbx", "rop_gltf", "rop_bgeo", "geometry",
}
# 这类节点只是配置（输出路径写在参数上），单独执行没有意义，不进列表
SKIP_TYPE_HINTS = ("settings", "materiallibrary", "lightmixer")
# 输出路径参数候选（按优先级）
PATH_PARMS = [
    "sopoutput", "filename", "picture", "vm_picture",
    "RS_outputFileNamePrefix", "ar_picture", "outputpath",
    "file", "usdfile", "lopoutput", "output",
]


def safe_int(s):
    try:
        return int(float(str(s).strip()))
    except Exception:
        return None


def expand_int(expr):
    """把 $FSTART 之类的表达式展开成 int，失败返回 None。"""
    import hou
    try:
        v = hou.expandString(expr)
        # 展开失败会原样返回，含 $ 就认为无效
        if "$" in v or v.strip() == "":
            return None
        return safe_int(v)
    except Exception:
        return None


def get_global_range():
    import hou
    res = {}
    for key, var in (("fstart", "$FSTART"), ("fend", "$FEND"),
                     ("rfstart", "$RFSTART"), ("rfend", "$RFEND")):
        res[key] = expand_int(var)
    # 兜底：用播放条范围
    try:
        a, b = hou.playbar.frameRange()
        if res.get("fstart") is None:
            res["fstart"] = int(a)
        if res.get("fend") is None:
            res["fend"] = int(b)
    except Exception:
        pass
    return res


def get_output_parm(node):
    for name in PATH_PARMS:
        p = node.parm(name)
        if p is not None:
            try:
                val = p.eval()
            except Exception:
                try:
                    val = p.evalAsString()
                except Exception:
                    continue
            # __render__.usd 之类是 Houdini 内部的临时文件，不是用户要的输出
            if val and not str(val).startswith("__render__"):
                return name, str(val)
    return None, None


def get_frame_range(node, glb):
    """优先取节点自身渲染范围，取不到返回全局。"""
    f1, f2 = node.parm("f1"), node.parm("f2")
    if f1 is not None and f2 is not None:
        try:
            return int(f1.eval()), int(f2.eval()), "node"
        except Exception:
            pass
    # 有些节点用 range start/end
    for a, b in (("range_start", "range_end"), ("start", "end")):
        pa, pb = node.parm(a), node.parm(b)
        if pa is not None and pb is not None:
            try:
                return int(pa.eval()), int(pb.eval()), "node"
            except Exception:
                pass
    # 默认按 $RFSTART~$RFEND（渲染范围），取不到才回退 $FSTART~$FEND
    a = glb.get("rfstart") if glb.get("rfstart") is not None else glb.get("fstart")
    b = glb.get("rfend") if glb.get("rfend") is not None else glb.get("fend")
    return a, b, "global"


def classify(node):
    """返回 'cache' / 'render' / None。"""
    try:
        tname = node.type().name().lower()
    except Exception:
        tname = ""
    if any(h in tname for h in SKIP_TYPE_HINTS):
        return None
    if tname in RENDER_TYPES:
        return "render"
    if tname in CACHE_TYPES:
        return "cache"
    # 启发式：看参数里有没有图像类输出
    pname, pval = get_output_parm(node)
    if pname in ("picture", "vm_picture", "ar_picture", "RS_outputFileNamePrefix"):
        return "render"
    if pname in ("sopoutput", "filename", "usdfile", "lopoutput"):
        low = (pval or "").lower()
        if any(low.endswith(ext) for ext in
               (".exr", ".png", ".jpg", ".jpeg", ".tif", ".tiff", ".dpx", ".hdr")):
            return "render"
        return "cache"
    return None


def walk(node):
    for c in node.children():
        yield c
        yield from walk(c)


def collect_from(container_path, glb, out, seen, sop_mode=False):
    """sop_mode=True 时只收 SOP 层的缓存节点（filecache 等），避免把
    /obj 下普通几何/相机节点误列进来。"""
    import hou
    root = hou.node(container_path)
    if root is None:
        return
    for n in walk(root):
        p = n.path()
        if p in seen:
            continue
        tname = ""
        try:
            tname = n.type().name().lower()
        except Exception:
            pass
        # SOP 网络里只认类型名白名单中的缓存/渲染节点，不走参数启发式，
        # 否则 file1/box/null 这类普通 SOP 会因为 filename 参数被误判为缓存
        if sop_mode:
            exact = tname in CACHE_TYPES or tname in RENDER_TYPES
            if not exact and not any(tname.startswith(h) for h in
                                     ("filecache", "alembic", "rop_geometry")):
                continue
        cat = classify(n)
        if cat is None:
            continue
        seen.add(p)
        outparm, outval = get_output_parm(n)
        fs, fe, src = get_frame_range(n, glb)
        out.append({
            "path": p,
            "type": n.type().name(),
            "category": cat,
            "output": outval,
            "output_parm": outparm,
            "fstart": fs,
            "fend": fe,
            "frame_source": src,
        })


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("hip")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    result = {"hip": args.hip, "houdini": None, "error": None,
              "global": {}, "nodes": []}
    sys.stdout.write("##BOOT\n")
    sys.stdout.flush()
    try:
        import hou
        result["houdini"] = hou.applicationVersionString()
        hou.hipFile.load(args.hip, suppress_save_prompt=True,
                         ignore_load_warnings=True)
        sys.stdout.write("##READY\n")
        sys.stdout.flush()

        glb = get_global_range()
        result["global"] = glb

        nodes = []
        seen = set()
        # ROP 输出在 /out；LOP 渲染在 /stage；File Cache SOP 在 /obj 下
        for container in ("/out", "/stage"):
            collect_from(container, glb, nodes, seen)
        collect_from("/obj", glb, nodes, seen, sop_mode=True)

        # 排序：缓存在前，渲染在后，再按路径
        order = {"cache": 0, "render": 1}
        nodes.sort(key=lambda x: (order.get(x["category"], 9), x["path"]))
        result["nodes"] = nodes
        sys.stdout.write("##NODES %d\n" % len(nodes))
        sys.stdout.flush()

    except Exception as e:
        import traceback
        result["error"] = "%s: %s" % (type(e).__name__, e)
        result["traceback"] = traceback.format_exc()

    text = json.dumps(result, ensure_ascii=False, indent=2)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        # 用标记包裹，方便 Rust 侧截取
        sys.stdout.write("<<<ANALYZE_JSON_START>>>\n")
        sys.stdout.write(text)
        sys.stdout.write("\n<<<ANALYZE_JSON_END>>>\n")


if __name__ == "__main__":
    main()
