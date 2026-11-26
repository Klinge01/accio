from io import BytesIO

import pytest


@pytest.fixture
def png_hallo():
    from PIL import Image, ImageDraw, ImageFont

    image = Image.effect_noise((900, 300), 23).point(lambda p: 220 + p // 8).convert("RGB")
    font = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial.ttf", 54)
    ImageDraw.Draw(image).text((45, 100), "Hallo Welt 2026", font=font, fill="black")
    output = BytesIO()
    image.save(output, format="PNG")
    return output.getvalue()


@pytest.fixture
def png_hallo_file(tmp_path, png_hallo):
    path = tmp_path / "hallo.png"
    path.write_bytes(png_hallo)
    return path


@pytest.fixture
def sample_pdf(tmp_path, png_hallo):
    import pymupdf

    path = tmp_path / "sample.pdf"
    with pymupdf.open() as document:
        document.new_page().insert_text((60, 100), "Mietvertrag Analyse der Wohnung")
        page = document.new_page()
        page.insert_image(pymupdf.Rect(0, 0, 590, 200), stream=png_hallo)
        document.save(path)
    return path


@pytest.fixture
def sample_docx(tmp_path):
    from docx import Document

    path = tmp_path / "sample.docx"
    document = Document()
    document.add_heading("Vertrag", level=1)
    document.add_paragraph("Die Vertragslaufzeit beträgt zwölf Monate.")
    document.save(path)
    return path


@pytest.fixture
def sample_pptx(tmp_path):
    from pptx import Presentation

    path = tmp_path / "sample.pptx"
    presentation = Presentation()
    for title in ("Agenda", "Preise"):
        slide = presentation.slides.add_slide(presentation.slide_layouts[5])
        slide.shapes.title.text = title
    presentation.save(path)
    return path


@pytest.fixture
def sample_xlsx(tmp_path):
    from openpyxl import Workbook

    path = tmp_path / "sample.xlsx"
    workbook = Workbook()
    sheet = workbook.active
    sheet.title = "Kunden"
    sheet.append(["Name", "Nummer"])
    for index in range(70):
        sheet.append([f"Kunde {index}", index])
    workbook.save(path)
    return path
