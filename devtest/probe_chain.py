import sys
import time
import hou

hip = sys.argv[1]
frame = int(sys.argv[2]) if len(sys.argv) > 2 else 1201
hou.hipFile.load(hip, suppress_save_prompt=True, ignore_load_warnings=True)
hou.setUpdateMode(hou.updateMode.AutoUpdate)
hou.setFrame(frame)
print("##VER", hou.applicationVersionString(), "frame", frame)

n = hou.node("/obj/Fx/filecache_smoke0")
# 顺着输入链往上走 8 层，逐个 cook 看在哪一层断掉
chain = []
cur = n
for _ in range(10):
    chain.append(cur)
    ins = [i for i in cur.inputs() if i]
    if not ins:
        break
    cur = ins[0]

if n.parm("loadfromdisk"):
    n.parm("loadfromdisk").set(0)

for node in chain:
    t0 = time.time()
    try:
        g = node.geometry()
        pts, prs = len(g.points()), len(g.prims())
    except Exception as e:
        pts = prs = -1
        print("  cook raised:", e)
    print("%-52s %-22s pts=%-7d prims=%-7d %.2fs" %
          (node.path(), node.type().name(), pts, prs, time.time() - t0))
    try:
        errs = node.errors()
        if errs:
            print("      ERR:", errs[0][:160])
    except Exception:
        pass
