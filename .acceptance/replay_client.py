"""Rebuild tools/hydra-py/hydra_sdk/client.py by replaying the write/edit calls
recorded in the DSH session transcripts (the file was destroyed by a stray
`git checkout`; git never had the content, but the session that authored it did)."""
import sys, json, glob, os

TARGET = "hydra_sdk/client.py"
ops = []
for path in glob.glob("/home/alex/.dsh/sessions/--home-alex-Projects-hydra--/*/session.jsonl.zstd"):
    import subprocess
    raw = subprocess.run(["zstd", "-dc", path], capture_output=True, text=True, errors="replace").stdout
    for line in raw.splitlines():
        if TARGET not in line:
            continue
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if obj.get("type") != "tool/call":
            continue
        data = obj.get("data", {})
        args_raw = data.get("arguments")
        if not isinstance(args_raw, str):
            continue
        try:
            args = json.loads(args_raw)
        except Exception:
            continue
        if not str(args.get("file_path", "")).endswith(TARGET):
            continue
        kind = "write" if "content" in args else ("edit" if "new_string" in args else None)
        if kind:
            ops.append((obj.get("time") or 0, path.split("/")[-2][:8], kind, args))
ops.sort(key=lambda o: o[0])
print(f"{len(ops)} write/edit operation(s) found across the recorded sessions")
for t, s, kind, a in ops:
    n = len(a.get("content", "") or a.get("new_string", ""))
    print(f"  {t} {s} {kind} len={n}")

text = None
applied = skipped = 0
for t, s, kind, a in ops:
    if kind == "write":
        text = a["content"]
        print(f"-> write from {s} at {t}: {len(text.splitlines())} lines")
        continue
    if text is None:
        print(f"   !! edit before any write ({s}); ignoring")
        continue
    old, new = a["old_string"], a["new_string"]
    if text.count(old) == 1:
        text = text.replace(old, new, 1)
        applied += 1
    else:
        skipped += 1
        print(f"   !! edit did not apply cleanly ({s} {t}): occurrences={text.count(old)} "
              f"old[:60]={old[:60]!r}")
print(f"applied={applied} skipped={skipped} final_lines={len(text.splitlines()) if text else 0}")
if "--write" in sys.argv and text:
    out = "/home/alex/Projects/hydra/tools/hydra-py/hydra_sdk/client.py"
    open(out, "w", encoding="utf-8").write(text)
    print("WROTE", out)
