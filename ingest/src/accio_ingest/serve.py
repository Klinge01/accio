import json
import sys
from contextlib import redirect_stdout
from pathlib import Path
from typing import TextIO

from .dispatch import parse_file


def serve(stdin: TextIO, stdout: TextIO):
    for line in stdin:
        request_id = None
        try:
            request = json.loads(line)
            request_id = request["id"]
            with redirect_stdout(sys.stderr):
                root = request.get("root")
                chunks = parse_file(
                    Path(request["path"]),
                    request.get("pdf_mode") or "text",
                    Path(root) if root else None,
                )
            response = {"id": request_id, "chunks": [c.to_json() for c in chunks], "error": None}
        except Exception as error:
            response = {"id": request_id, "chunks": [], "error": f"{type(error).__name__}: {error}"}
        print(json.dumps(response, ensure_ascii=True), file=stdout, flush=True)
