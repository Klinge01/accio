import re
import shutil
import subprocess
from pathlib import Path
from tempfile import TemporaryDirectory

from .chunker import chunk_markdown
from .types import Chunk

SLIDE = re.compile(r"<!--\s*Slide number:\s*(\d+)\s*-->", re.I)
SHEET = re.compile(r"^##[ \t]+(.+?)\s*$", re.M)


def _markdown(path: Path) -> str:
    from markitdown import MarkItDown

    return MarkItDown().convert(path).text_content


def parse_docx(path: Path) -> list[Chunk]:
    return chunk_markdown(_markdown(path), path.name)


def parse_pptx(path: Path) -> list[Chunk]:
    parts = SLIDE.split(_markdown(path))
    chunks = []
    for number, body in zip(parts[1::2], parts[2::2]):
        lines = body.strip().splitlines()
        heading = next((line for line in lines if line.startswith("# ")), None)
        title = (heading or (lines[0] if lines else "")).removeprefix("# ").strip()
        header = f"{path.name} › Slide {number}"
        if title:
            header += f": {title}"
        if lines:
            chunks.append(Chunk(header, body.strip(), int(number)))
    return chunks


XLSX_ROWS = 30
XLSX_MAX_CHARS = 1800


def _slices(text: str, size: int) -> list[str]:
    return [text[i:i + size] for i in range(0, len(text), size)] or [""]


def parse_xlsx(path: Path) -> list[Chunk]:
    """Markdown table rows packed up to 30 rows / 1800 chars per chunk, header row repeated.
    Wide sheets (e.g. surveys with hundreds of columns) are sliced instead of producing
    100k-character chunks: an over-wide header is emitted once on its own, an over-wide row
    is split across several chunks."""
    parts = SHEET.split(_markdown(path))
    chunks = []
    for name, body in zip(parts[1::2], parts[2::2]):
        header = f"{path.name} › {name}"
        sheet_start = len(chunks)
        rows = [line for line in body.splitlines() if line.lstrip().startswith("|")]
        columns, data = "\n".join(rows[:2]), rows[2:]
        if len(columns) > XLSX_MAX_CHARS // 2:
            chunks += [Chunk(header, piece) for piece in _slices(columns, XLSX_MAX_CHARS)]
            columns = ""
        prefix = columns + "\n" if columns else ""
        budget = XLSX_MAX_CHARS - len(prefix)
        batch: list[str] = []

        def flush():
            if batch or (prefix and len(chunks) == sheet_start):
                chunks.append(Chunk(header, prefix + "\n".join(batch)))
            batch.clear()

        for row in data:
            if len(row) > budget:
                flush()
                chunks.extend(Chunk(header, prefix + piece) for piece in _slices(row, budget))
                continue
            if len(batch) == XLSX_ROWS or len("\n".join(batch + [row])) > budget:
                flush()
            batch.append(row)
        flush()
    return chunks


def parse_doc(path: Path) -> list[Chunk]:
    result = subprocess.run(
        ["textutil", "-convert", "txt", "-stdout", str(path)],
        check=True, capture_output=True, encoding="utf-8", errors="replace",
    )
    return chunk_markdown(result.stdout, path.name)


def parse_ppt(path: Path) -> list[Chunk]:
    executable = shutil.which("soffice")
    if executable is None:
        raise RuntimeError("LibreOffice not installed (brew install --cask libreoffice)")
    with TemporaryDirectory() as temporary:
        profile = (Path(temporary) / "profile").as_uri()
        subprocess.run(
            [executable, f"-env:UserInstallation={profile}", "--headless", "--convert-to",
             "pptx", "--outdir", temporary, str(path)],
            check=True, capture_output=True,
        )
        converted = Path(temporary) / f"{path.stem}.pptx"
        chunks = parse_pptx(converted)
        return [Chunk(path.name + chunk.header[len(converted.name):], chunk.text, chunk.page)
                for chunk in chunks]
