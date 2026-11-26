import shutil
import subprocess

import pytest

from accio_ingest.office import parse_doc, parse_docx, parse_ppt, parse_pptx, parse_xlsx


def test_docx_heading_header(sample_docx):
    assert parse_docx(sample_docx)[0].header == "sample.docx › Vertrag"


def test_pptx_one_chunk_per_slide(sample_pptx):
    chunks = parse_pptx(sample_pptx)
    assert [c.header for c in chunks] == ["sample.pptx › Slide 1: Agenda", "sample.pptx › Slide 2: Preise"]
    assert [c.page for c in chunks] == [1, 2]


def test_xlsx_rows_chunked_with_header(sample_xlsx):
    chunks = parse_xlsx(sample_xlsx)
    assert len(chunks) == 3
    assert all(c.header == "sample.xlsx › Kunden" for c in chunks)
    assert all(c.text.splitlines()[0].startswith("| Name") for c in chunks)
    assert [len(c.text.splitlines()) for c in chunks] == [32, 32, 12]


def test_doc_via_textutil(tmp_path):
    text = tmp_path / "old.txt"
    text.write_text("Altes Dokument", encoding="utf-8")
    subprocess.run(["textutil", "-convert", "doc", str(text)], check=True)
    assert "Altes Dokument" in parse_doc(text.with_suffix(".doc"))[0].text


def test_ppt_without_soffice_raises(monkeypatch, tmp_path):
    monkeypatch.setattr(shutil, "which", lambda _: None)
    with pytest.raises(RuntimeError, match=r"LibreOffice not installed \(brew install --cask libreoffice\)"):
        parse_ppt(tmp_path / "x.ppt")


def test_xlsx_wide_sheet_chunks_stay_within_budget(tmp_path):
    from openpyxl import Workbook

    book = Workbook()
    sheet = book.active
    sheet.title = "Umfrage"
    sheet.append([f"Frage {i}: Wie zufrieden sind Sie mit Aspekt {i}?" for i in range(300)])
    for row in range(5):
        sheet.append([f"Antwort {row}-{i}" for i in range(300)])
    path = tmp_path / "wide.xlsx"
    book.save(path)
    chunks = parse_xlsx(path)
    assert chunks and all(len(c.text) <= 1800 for c in chunks)
    joined = " ".join(c.text for c in chunks)
    assert "Frage 299" in joined and "Antwort 4-299" in joined  # nothing dropped
