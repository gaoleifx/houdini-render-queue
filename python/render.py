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
import uuid
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

    # 探针文件名必须唯一：多台机器（或同一台机器的多个任务）同时预检同一个
    # 输出目录时，固定名字会互相抢文件，后到的那台会误报「目录不可写」。
    probe = os.path.join(
        outdir, ".hrq_write_test_%s" % uuid.uuid4().hex[:12]
    )
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


def disable_loadfromdisk(node):
    """把 SOP 缓存节点的「Load From Disk」关掉，返回是否真的改过。

    File Cache SOP（filecache::2.0）开着 Load From Disk 时，cook 出来的不是
    上游算出来的几何，而是「去磁盘读缓存文件」：新版本目录里还没有文件，读到的
    就是空几何。这时拿 geometry ROP 去「渲染」它，等于把空的 902 字节 bgeo
    写回缓存路径，整段缓存全是 0 点——正是「缓存出来都是空的」的原因。
    所以真正写缓存之前必须先关掉它，让节点去跑上游。
    """
    p = node.parm("loadfromdisk")
    if p is None:
        return False
    try:
        if not p.eval():
            return False
        p.set(0)
        return True
    except Exception:
        return False


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

    # SOP 缓存节点：cook 之前先关掉 Load From Disk，否则节点不会去跑上游，
    # 只会读磁盘上已有的缓存（新版本目录里还没有文件 → 空几何），
    # 代理 ROP 再把这份空几何写回缓存路径，整段缓存都是 0 点。只改内存，不存 hip。
    if isinstance(node, hou.SopNode) and disable_loadfromdisk(node):
        emit("##NOTE",
             "节点原本是 Load From Disk 状态，已临时关闭后再缓存：%s" % node.path())

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
    sizes = []
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
        if proxy is not None:
            # 记下这一帧真实写出来的文件大小，用来兜住「写出一堆空几何」的情况
            try:
                outp = proxy.parm("sopoutput").evalAsString()
                sizes.append(os.path.getsize(outp) if os.path.exists(outp) else -1)
            except Exception:
                pass
        emit("##DONE", f)

    if canceled:
        emit("##CANCELED")
        return 4

    # 空的 bgeo.sc 约 900 字节。整段缓存全都这么小，等于什么都没缓存出来，
    # 与其报「完成」让人后面渲染才发现没东西，不如当成失败喊出来。
    if proxy is not None and len(sizes) >= 2 and all(0 <= s < 1024 for s in sizes):
        emit("##FAIL",
             "全部 %d 帧的输出都小于 1KB，缓存内容是空的（请检查该节点上游"
             "是否有解算/几何数据，或 Load From Disk 是否仍未关闭）" % len(sizes))
        return 3

    emit("##FINISH")
    return 0


if __name__ == "__main__":
    sys.exit(main())
