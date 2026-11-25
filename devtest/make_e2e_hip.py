"""把生产 hip 的 filecache_smoke0 输出改到 devtest/out/e2e，存成临时 hip 供端到端验证。"""
import os
import sys
import hou

src = sys.argv[1]
dst = sys.argv[2]
basedir = sys.argv[3]

hou.hipFile.load(src, suppress_save_prompt=True, ignore_load_warnings=True)
n = hou.node("/obj/Fx/filecache_smoke0")
n.parm("basedir").set(basedir)
n.parm("version").set(1)
print("basedir ->", n.parm("basedir").evalAsString())
print("sopoutput ->", n.parm("sopoutput").evalAsString())
print("loadfromdisk =", n.parm("loadfromdisk").eval())
hou.hipFile.save(dst)
print("saved", dst, os.path.getsize(dst))
