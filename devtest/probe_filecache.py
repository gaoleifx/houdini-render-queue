import sys
import hou

hip = sys.argv[1]
hou.hipFile.load(hip, suppress_save_prompt=True, ignore_load_warnings=True)
print("##VER", hou.applicationVersionString())
print("##PREF", hou.getenv("HOUDINI_USER_PREF_DIR"))
for t in ("Sop/axiom_solver::3.3", "Sop/axiom_solver_core::3.3"):
    print("TYPE %-28s %s" % (t, "OK" if hou.nodeType(t) else "MISSING"))

KEYS = ("loadfromdisk", "enable", "file", "sopoutput", "basedir", "basename",
        "version", "trange", "f1", "f2", "f3", "f4", "mkpath", "render")
targets = ["/obj/Fx/filecache_smoke0", "/obj/Fx/filecache_tex0",
           "/obj/Fx/filecache_source0", "/obj/Fx/vellum2"]

for path in targets:
    n = hou.node(path)
    print("=" * 50)
    if n is None:
        print("NODE MISSING", path)
        continue
    print("NODE", path, "type=", n.type().name())
    for nm in KEYS:
        p = n.parm(nm)
        if p is None:
            continue
        try:
            v = p.eval()
        except Exception as e:
            v = "<eval err %s>" % e
        print("  %-13s = %r" % (nm, v))
    rel = [p.name() for p in n.parms()
           if any(k in p.name().lower() for k in ("disk", "load", "cach", "write"))]
    print("  related:", rel)
    print("  inputs:", [i.path() if i else None for i in n.inputs()])

    # 在第 1300 帧 cook 一下，看输出到底有没有点
    hou.setFrame(1300)
    try:
        geo = n.geometry()
        print("  COOK frame1300 points=%d prims=%d" % (len(geo.points()), len(geo.prims())))
    except Exception as e:
        print("  COOK failed:", e)
    try:
        errs = n.errors()
        print("  errors:", errs[:3] if errs else "none")
    except Exception:
        pass
