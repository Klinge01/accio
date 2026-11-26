import json
import sys
import time
from pathlib import Path

for line in sys.stdin:
    request = json.loads(line)
    path = Path(request["path"])
    error = None
    chunks = []
    if "slow" in path.name:
        time.sleep(10)
    if "crash" in path.name:
        sys.exit(1)
    if "bad" in path.name:
        error = "ValueError: bad file"
    else:
        text = path.read_text()
        if path.suffix.lower() == ".pdf":
            text += f" pdfmode{request.get('pdf_mode')}"
        chunks = [{"header": path.stem, "text": text, "page": None}]
        if request.get("root"):
            # Code repository: header relative to the root, line range of the file.
            header = str(path.relative_to(Path(request["root"]).resolve()))
            lines = len(text.splitlines()) or 1
            chunks = [{"header": header, "text": text, "page": None, "line": 1, "end_line": lines}]
    print(json.dumps({"id": request["id"], "chunks": chunks, "error": error}), flush=True)
