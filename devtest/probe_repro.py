"""复现 render.py 的代理 ROP 写盘流程，对比 filecache 的 loadfromdisk 开关。"""
import os
import sys
import time
import hou

hip = sys.argv[1]
frame = int(sys.argv[2]) if len(sys.argv) > 2 else 1201
outdir = os.path.dirname(os.path.abspath(__file__)) + "/out"
os.makedirs(outdir, exist_ok=True)

hou.hipFile.load(hip, suppress_save_prompt=True, ignore_load_warnings=True)
hou.setUpdateMode(hou.updateMode.Manual)   # 和 render.py 一致
print("##VER", hou.applicationVersionString())

node = hou.node("/obj/Fx/filecache_smoke0")
out_root = hou.node("/out")


def run(tag, loadfromdisk):
    node.parm("loadfromdisk").set(loadfromdisk)
    proxy = out_root.createNode("geometry", "hrq_probe_" + tag)
    proxy.parm("soppath").set(node.path())
    target = "%s/repro_%s.$F4.bgeo.sc" % (outdir.replace("\\", "/"), tag)
    proxy.parm("sopoutput").set(target)
    try:
        proxy.parm("mkpath").set(1)
    except Exception:
        pass
    proxy.parm("f1").set(frame)
    proxy.parm("f2").set(frame)
    t0 = time.time()
    proxy.render(frame_range=(frame, frame))
    dt = time.time() - t0
    path = target.replace("$F4", "%04d" % frame)
    size = os.path.getsize(path) if os.path.exists(path) else -1
    pts = prs = -1
    if size > 0:
        g = hou.node("/obj").createNode("geo", "chk_" + tag)
        f = g.createNode("file")
        f.parm("file").set(path)
        geo = f.geometry()
        pts, prs = len(geo.points()), len(geo.prims())
        g.destroy()
    print("RESULT %-14s loadfromdisk=%d  %.1fs  size=%-10d points=%-6d prims=%d"
          % (tag, loadfromdisk, dt, size, pts, prs))
    proxy.destroy()


run("ld1", 1)
run("ld0", 0)
print("##DONE")
