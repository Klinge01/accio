import io
import json

from accio_ingest.serve import serve


def test_serve_roundtrip(tmp_path):
    markdown = tmp_path / "a.md"
    markdown.write_text("# H\nhello", encoding="utf-8")
    output = io.StringIO()
    serve(io.StringIO(json.dumps({"id": 7, "path": str(markdown)}) + "\n"), output)
    assert json.loads(output.getvalue()) == {
        "id": 7, "chunks": [{"header": "a › H", "text": "hello", "page": None, "line": None, "end_line": None}], "error": None,
    }


def test_serve_error_does_not_kill_worker(tmp_path):
    requests = [{"id": index, "path": str(tmp_path / name)}
                for index, name in enumerate(["x.zzz", "missing.md"], 1)]
    output = io.StringIO()
    serve(io.StringIO("\n".join(map(json.dumps, requests)) + "\n"), output)
    first, second = map(json.loads, output.getvalue().splitlines())
    assert first["error"] == "ValueError: unsupported extension .zzz"
    assert second["error"].startswith("FileNotFoundError")


def test_serve_passes_pdf_mode(tmp_path, monkeypatch):
    import accio_ingest.serve as serve_module

    seen = []
    monkeypatch.setattr(
        serve_module, "parse_file", lambda path, pdf_mode="text", root=None: seen.append((pdf_mode, root)) or []
    )
    reqs = [
        {"id": 1, "path": str(tmp_path / "a.pdf"), "pdf_mode": "markdown"},
        {"id": 2, "path": str(tmp_path / "b.pdf")},
        {"id": 3, "path": str(tmp_path / "C.java"), "root": str(tmp_path)},
    ]
    serve(io.StringIO("\n".join(map(json.dumps, reqs)) + "\n"), io.StringIO())
    assert seen == [("markdown", None), ("text", None), ("text", tmp_path)]
