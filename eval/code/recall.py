#!/usr/bin/env python3
"""Retrieval check for a code repository: for each question, is a key file among the
top-k code_search hits? Talks to the running Accio app over its socket.

    python3 eval/code/recall.py /path/to/repo [eval/code/questions.jsonl] [--k 8]
"""
import argparse
import json
import os
import socket
import time
from pathlib import Path

HERE = Path(__file__).parent


def socket_path() -> Path:
    if os.environ.get("ACCIO_SOCKET"):
        return Path(os.environ["ACCIO_SOCKET"])
    home = os.environ.get("ACCIO_HOME") or Path.home() / "Library/Application Support/accio"
    return Path(home) / "accio.sock"


def search(repo: Path, query: str, limit: int) -> dict:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.connect(str(socket_path()))
        request = {"op": "code_search", "repo": str(repo), "query": query, "limit": limit}
        client.sendall((json.dumps(request) + "\n").encode())
        data = b""
        while not data.endswith(b"\n"):
            part = client.recv(65536)
            if not part:
                break
            data += part
    response = json.loads(data)
    if not response.get("ok"):
        raise RuntimeError(response.get("error"))
    return response["result"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("repo", type=Path)
    parser.add_argument("questions", type=Path, nargs="?", default=HERE / "questions.jsonl")
    parser.add_argument("--k", type=int, default=8)
    args = parser.parse_args()
    rows = [json.loads(line) for line in args.questions.read_text().splitlines() if line.strip()]
    found_any = found_first = 0
    for row in rows:
        started = time.perf_counter()
        result = search(args.repo, row["q"], args.k)
        ms = (time.perf_counter() - started) * 1000
        paths = [hit["path"] for hit in result["hits"]]
        keys = row["key_files"]
        ranks = [paths.index(key) + 1 for key in keys if key in paths]
        found_any += bool(ranks)
        found_first += keys[0] in paths
        rank = min(ranks) if ranks else "-"
        print(f"{row['id']} {row['kind']:<8} best key rank {rank!s:>2} · {len(ranks)}/{len(keys)} keys · {ms:5.0f} ms  {row['q'][:70]}")
    total = len(rows)
    print(f"\nrecall@{args.k}: any key file {found_any}/{total}, main key file {found_first}/{total}")


if __name__ == "__main__":
    main()
