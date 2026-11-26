from pathlib import Path

from .chunker import chunk_markdown
from .images import parse_image
from .office import parse_doc, parse_docx, parse_ppt, parse_pptx, parse_xlsx
from .pdf import parse_pdf
from .types import Chunk


def _parse_md(path: Path) -> list[Chunk]:
    return chunk_markdown(path.read_text(encoding="utf-8", errors="replace"), path.stem)


PARSERS = {
    ".md": _parse_md, ".pdf": parse_pdf, ".doc": parse_doc, ".docx": parse_docx,
    ".ppt": parse_ppt, ".pptx": parse_pptx, ".xlsx": parse_xlsx,
    **dict.fromkeys((".png", ".jpg", ".jpeg", ".heic", ".webp"), parse_image),
}


def parse_file(path: Path, pdf_mode: str = "text", root: Path | None = None) -> list[Chunk]:
    """Chunks of one file. `root`: the file belongs to the code repository at `root`."""
    if root is not None:
        from .code import parse_code

        return parse_code(path, root)
    ext = path.suffix.lower()
    if ext == ".pdf":
        return parse_pdf(path, pdf_mode)
    parser = PARSERS.get(ext)
    if parser is None:
        raise ValueError(f"unsupported extension {ext}")
    return parser(path)
