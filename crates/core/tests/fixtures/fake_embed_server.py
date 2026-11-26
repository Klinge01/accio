"""Stand-in for `accio-ingest embed-server` (stdlib only).

Modes: ok | crash-second (exit on the 2nd request; a respawned process works) | error.
Vectors: 768 dims derived from each text's bytes, base64 little-endian f32.
"""
import base64
import json
import os
import struct
import sys
import tempfile

mode = sys.argv[1]
marker = os.path.join(tempfile.gettempdir(), f"fake-embed-crashed-{os.getppid()}")
count = 0
for line in sys.stdin:
    request = json.loads(line)
    count += 1
    if mode == "crash-second" and count == 2 and not os.path.exists(marker):
        open(marker, "w").close()
        sys.exit(1)
    if "translate" in request:
        print(json.dumps({"id": request["id"], "text": f"{request['to']}:{request['translate']}"}), flush=True)
        continue
    if mode == "error":
        print(json.dumps({"id": request["id"], "error": "boom"}), flush=True)
        continue
    values = []
    for text in request["texts"]:
        seed = sum(text.encode()) or 1
        values += [((seed * (i + 1)) % 97) / 97.0 + 0.01 for i in range(768)]
    payload = base64.b64encode(struct.pack(f"<{len(values)}f", *values)).decode()
    print(json.dumps({"id": request["id"], "n": len(request["texts"]), "dim": 768, "f32le": payload}), flush=True)
