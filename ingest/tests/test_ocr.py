from accio_ingest.ocr import ocr_image


def test_ocr_reads_rendered_text(png_hallo):
    assert "Hallo" in ocr_image(png_hallo)
