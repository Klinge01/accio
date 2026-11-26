from pathlib import Path

from .chunker import chunk_markdown
from .ocr import ocr_image
from .types import Chunk


def parse_pdf(path: Path, mode: str = "text") -> list[Chunk]:
    """mode "text": plain extraction (~7 ms/page). mode "markdown": pymupdf4llm, which
    keeps tables and headings (~125 ms/page). Pages without a text layer are OCR'd."""
    import pymupdf

    if mode not in ("text", "markdown"):
        raise ValueError(f"unknown pdf_mode {mode!r}")
    chunks = []
    with pymupdf.open(path) as document:
        if document.needs_pass:
            raise ValueError("encrypted PDF requires a password")
        if mode == "markdown":
            import pymupdf4llm

            pages = [p["text"] for p in pymupdf4llm.to_markdown(document, page_chunks=True, use_ocr=False)]
        else:
            pages = [page.get_text("text", sort=True) for page in document]
        for index, text in enumerate(pages):
            number = index + 1
            source_page = document[index]
            if len(text.strip()) < 20 or len(source_page.get_text().strip()) < 20:
                text = ocr_image(source_page.get_pixmap(dpi=150).tobytes("png"))
            chunks.extend(chunk_markdown(text, path.name, number, f"{path.name} › p.{number}"))
    return chunks
