import argparse
import json
import os
import sys
from contextlib import redirect_stdout
from pathlib import Path

from .dispatch import parse_file
from .serve import serve


def main():
    parser = argparse.ArgumentParser(prog="accio-ingest")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("serve")
    commands.add_parser("parse").add_argument("path", type=Path)
    commands.add_parser("embed-server", help="resident GPU embedding helper for the app")
    commands.add_parser("fetch-models", help="download the image caption model")
    args = parser.parse_args()

    if args.command == "fetch-models":
        from .caption import fetch_model

        print(fetch_model())
        return

    # Preserve the original pipe for JSON. Native libraries also write to fd 1.
    with os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8") as output:
        os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
        with redirect_stdout(sys.stderr):
            if args.command == "serve":
                serve(sys.stdin, output)
            elif args.command == "embed-server":
                from .embed_server import serve_embeddings

                serve_embeddings(sys.stdin, output)
            else:
                json.dump([chunk.to_json() for chunk in parse_file(args.path)], output, ensure_ascii=False)
                output.write("\n")


if __name__ == "__main__":
    main()
