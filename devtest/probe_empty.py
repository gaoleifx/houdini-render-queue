import sys
import hou

print("##VER", hou.applicationVersionString())

for t in ("Sop/axiom_solver::3.3", "Sop/axiom_solver_core::3.3",
          "Sop/filecache::2.0"):
    try:
        nt = hou.nodeType(t)
    except Exception as e:
        nt = None
        print("nodeType(%s) raised %s" % (t, e))
    print("type %-28s -> %s" % (t, "OK" if nt else "MISSING"))

# 检查一个今天队列写出来的缓存到底是不是空几何
import os
paths = [
    r"Z:/b30/b30010/efx/task/houdini/hip/geo/Fx_filecache_smoke0/v11/Fx_filecache_smoke0_v11.1300.bgeo.sc",
    r"Z:/b30/b30010/efx/task/houdini/hip/geo/Fx_filecache_smoke0/v10/Fx_filecache_smoke0_v10.1300.bgeo.sc",
    r"Z:/b30/b30010/efx/task/houdini/hip/geo/Fx_filecache_smoke0/v3/Fx_filecache_smoke0_v3.1300.bgeo.sc",
]
obj = hou.node("/obj")
for p in paths:
    if not os.path.exists(p):
        print("MISSING FILE", p)
        continue
    g = obj.createNode("geo", "probe_geo")
    f = g.createNode("file")
    f.parm("file").set(p)
    try:
        geo = f.geometry()
        pts = geo.points()
        prims = geo.prims()
        vdb = 0
        try:
            vdb = len(geo.primFloatAttribs())
        except Exception:
            pass
        print("FILE %-70s size=%9d points=%6d prims=%6d" %
              (os.path.basename(os.path.dirname(p)) + "/" + os.path.basename(p),
               os.path.getsize(p), len(pts), len(prims)))
    except Exception as e:
        print("FILE %s cook failed: %s" % (p, e))
    g.destroy()
