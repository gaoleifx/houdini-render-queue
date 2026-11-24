#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
render.py — 在 hython 中按帧执行缓存/渲染，逐帧回报进度，支持断点续渲。

用法:
    hython render.py <hip> --node /out/geo_cache --plan plan.json [--cancel cancel.flag]

plan.json: {"frames": [1,2,3,10,11]}

输出标记（Rust 侧解析）:
    ##BOOT                          进程已启动
    ##READY                         工程加载完成，节点已定位
    ##PREFLIGHT <reason>            预检未通过
    ##FRAME <f>                     开始渲染某帧
    ##DONE <f>                      某帧完成
    ##FAIL <reason>                 致命错误
    ##FINISH                        全部完成

退出码: 0 成功 / 2 预检失败 / 3 渲染失败 / 4 被取消
"""
import sys
import os
import json
import argparse

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

PATH_PARMS = [
    "sopoutput", "filename", "picture", "vm_picture",
    "RS_outputFileNamePrefix", "ar_picture", "outputpath",
    "file", "usdfile", "lopoutput", "output",
]

MIN_FREE_BYTES = 500 * 1024 * 1024  # 低于 500MB 拒绝开渲


def emit(tag, *rest):
    msg = tag + ((" " + " ".join(str(r) for r in rest)) if rest else "")
    print(msg, flush=True)


def get_output_path(node, frame):
    import hou
    for name in PATH_PARMS:
        p = node.parm(name)
        if p is None:
            continue
        try:
            raw = p.eval()
        except Exception:
            continue
        if not raw:
            continue
        try:
            hou.setFrame(frame)
            expanded = hou.expandString(str(raw))
        except Exception:
            expanded = str(raw)
        if expanded:
            return name, expanded
    return None, None


def preflight(node, fstart):
    """返回 (ok, reason)。检查输出目录可写、磁盘剩余空间。"""
    import hou
    import shutil

    pname, path = get_output_path(node, fstart)
    if not path:
        # 没有输出路径的节点（例如纯 USD 描述）跳过目录检查
        return True, None

    if "$" in path:
        return False, "输出路径存在无法展开的变量: %s" % path

    outdir = os.path.dirname(os.path.abspath(path))
    if not outdir:
        return True, None

    try:
        os.makedirs(outdir, exist_ok=True)
    except Exception as e:
        return False, "无法创建输出目录 %s (%s)" % (outdir, e)

    probe = os.path.join(outdir, ".hrq_write_test")
    try:
        with open(probe, "w") as f:
            f.write("ok")
        os.remove(probe)
    except Exception as e:
        return False, "输出目录不可写 %s (%s)" % (outdir, e)

    try:
        free = shutil.disk_usage(outdir).free
        if free < MIN_FREE_BYTES:
            return False, "磁盘空间不足: %s 剩余 %.1f GB" % (outdir, free / 1024 ** 3)
    except Exception:
        pass

    return True, None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("hip")
    ap.add_argument("--node", required=True)
    ap.add_argument("--plan", required=True)
    ap.add_argument("--cancel", default=None)
    args = ap.parse_args()

    emit("##BOOT")

    try:
        with open(args.plan, "r", encoding="utf-8") as f:
            plan = json.load(f)
        frames = [int(x) for x in plan.get("frames", [])]
    except Exception as e:
        emit("##FAIL", "读取帧计划失败: %s" % e)
        return 3

    if not frames:
        emit("##FINISH")
        return 0

    import hou
    try:
        hou.hipFile.load(args.hip, suppress_save_prompt=True,
                         ignore_load_warnings=True)
    except Exception as e:
        emit("##FAIL", "加载工程失败: %s" % e)
        return 3

    try:
        hou.setUpdateMode(hou.updateMode.Manual)
    except Exception:
        pass

    node = hou.node(args.node)
    if node is None:
        emit("##FAIL", "工程中找不到节点 %s" % args.node)
        return 3

    emit("##READY")

    ok, reason = preflight(node, frames[0])
    if not ok:
        emit("##PREFLIGHT", reason)
        return 2

    # SOP 缓存节点（如 filecache::2.0）没有 render()，需要借一个临时
    # geometry ROP 指向它逐帧写盘；ROP/LOP 节点照旧直接 render()
    is_sop_cache = False
    try:
        is_sop_cache = isinstance(node, hou.SopNode) and not hasattr(node, "render")
    except Exception:
        is_sop_cache = False
    proxy = None
    if is_sop_cache:
        try:
            pname, outval = get_output_path(node, frames[0])
            if not outval:
                emit("##FAIL", "缓存节点没有可用的输出路径")
                return 3
            # 展开结果形如 ..._v1.0001.bgeo.sc，把帧号换回 $F4 模板。
            # 帧号是紧跟在扩展名复合体（.bgeo.sc / .abc / .bgeo）前的 4 位数字。
            import re
            template = re.sub(r"\.(\d{4})(\.(?:bgeo|abc|usd|usdz|geo)(?:\.sc|\.gz)?)$",
                              r".$F4\2", outval)
            if template == outval:
                # 兜底：无标准扩展名时，取文件名里最后一段纯 4 位数字
                template = re.sub(r"(\d{4})(?=[^/\\]*$)", "$F4", outval, count=1)
            out_root = hou.node("/out")
            if out_root is None:
                emit("##FAIL", "工程中缺少 /out 容器，无法创建缓存代理")
                return 3
            proxy = out_root.createNode("geometry", "hrq_cache_proxy")
            proxy.parm("soppath").set(node.path())
            proxy.parm("sopoutput").set(template)
            try:
                proxy.parm("mkpath").set(1)
            except Exception:
                pass
            emit("##PROXY", node.path(), "->", template)
        except Exception as e:
            import traceback
            traceback.print_exc()
            emit("##FAIL", "创建缓存代理失败: %s" % e)
            return 3

    canceled = False
    for f in frames:
        if args.cancel and os.path.exists(args.cancel):
            canceled = True
            break
        emit("##FRAME", f)
        try:
            if proxy is not None:
                proxy.parm("f1").set(f)
                proxy.parm("f2").set(f)
                proxy.render(frame_range=(f, f))
            else:
                node.render(frame_range=(f, f))
        except Exception as e:
            import traceback
            traceback.print_exc()
            emit("##FAIL", "帧 %d 渲染失败: %s" % (f, e))
            return 3
        emit("##DONE", f)

    if canceled:
        emit("##CANCELED")
        return 4

    emit("##FINISH")
    return 0


if __name__ == "__main__":
    sys.exit(main())
