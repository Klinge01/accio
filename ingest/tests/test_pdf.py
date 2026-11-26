from accio_ingest.pdf import parse_pdf


def test_pdf_pages_and_ocr_fallback(sample_pdf):
    chunks = parse_pdf(sample_pdf)
    assert chunks[0].header.startswith("sample.pdf › p.1")
    assert "Mietvertrag" in chunks[0].text
    assert any(c.page == 2 and "Hallo" in c.text for c in chunks)


def test_markdown_mode_uses_pymupdf4llm_and_keeps_ocr_fallback(sample_pdf, monkeypatch):
    import pymupdf4llm

    calls = []
    real = pymupdf4llm.to_markdown

    def spy(*args, **kwargs):
        calls.append(kwargs)
        return real(*args, **kwargs)

    monkeypatch.setattr(pymupdf4llm, "to_markdown", spy)
    chunks = parse_pdf(sample_pdf, mode="markdown")
    assert calls, "markdown mode must use pymupdf4llm"
    assert "Mietvertrag" in chunks[0].text
    assert any(c.page == 2 and "Hallo" in c.text for c in chunks)
    calls.clear()
    parse_pdf(sample_pdf)  # default: plain text
    assert calls == []


def test_unknown_pdf_mode_rejected(sample_pdf):
    import pytest

    with pytest.raises(ValueError, match="unknown pdf_mode"):
        parse_pdf(sample_pdf, mode="html")
