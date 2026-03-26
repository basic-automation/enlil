import pathlib
p = pathlib.Path(r"D:\Development\enlil\enlil-platform\src\sync\mod.rs")
c = p.read_text(encoding="utf-8")
old = "let guard = lock.lock();\n        let guard = cvar.wait_while(guard,"
new = "let guard = cvar.wait_while(lock.lock(),"
c = c.replace(old, new)
p.write_text(c, encoding="utf-8")
print("done")
