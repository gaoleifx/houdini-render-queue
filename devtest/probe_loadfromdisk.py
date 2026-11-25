import sys
import time
import hou

hip = sys.argv[1]
frame = int(sys.argv[2]) if len(sys.argv) > 2 else 1100
hou.hipFile.load(hip, suppress_save_prompt=True, ignore_load_warnings=True)
print("##VER", hou.applicationVersionString())
hou.setUpdateMode(hou.updateMode.Manual)

n = hou.node("/obj/Fx/filecache_smoke0")
print("node:", n.path(), n.type().name())

# 1) 现状：loadfromdisk=1，且该帧磁盘上没有缓存文件
hou.setFrame(frame)
t0 = time.time()
g = n.geometry()
print("MODE1 loadfromdisk=%s frame=%d points=%d prims=%d  %.2fs" %
      (n.parm("loadfromdisk").eval(), frame, len(g.points()), len(g.prims()),
       time.time() - t0))
print("  errors:", n.errors()[:2] if n.errors() else "none")

# 2) 只改内存里的开关，不存盘：让它真正跑上游
n.parm("loadfromdisk").set(0)
t0 = time.time()
try:
    g2 = n.geometry()
    print("MODE2 loadfromdisk=0 frame=%d points=%d prims=%d  %.2fs" %
          (frame, len(g2.points()), len(g2.prims()), time.time() - t0))
    print("  errors:", n.errors()[:2] if n.errors() else "none")
    if len(g2.prims()):
        pr = g2.prims()[0]
        try:
            print("  first prim:", pr.type().name(), pr.type().category().name())
        except Exception as e:
            print("  first prim info err", e)
except Exception as e:
    print("MODE2 cook failed:", e)
