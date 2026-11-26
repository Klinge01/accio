import pytest

from accio_ingest.caption import caption_image
from accio_ingest.images import parse_image


def test_small_image_skipped(tmp_path):
    from PIL import Image

    path = tmp_path / "icon.png"
    Image.new("RGB", (64, 64)).save(path)
    assert parse_image(path) == []


def test_image_ocr_without_caption(monkeypatch, png_hallo_file):
    monkeypatch.delenv("ACCIO_CAPTION", raising=False)  # captions are opt-in
    [chunk] = parse_image(png_hallo_file)
    assert chunk.header == "hallo.png (image)"
    assert "Text in image:" in chunk.text and "Hallo" in chunk.text


@pytest.mark.slow
def test_caption_nonempty(png_hallo_file):
    assert len(caption_image(png_hallo_file)) > 10


def test_missing_caption_model_fails_fast_without_download(monkeypatch, png_hallo_file):
    import time

    from huggingface_hub.errors import LocalEntryNotFoundError

    from accio_ingest import caption

    calls = []

    def offline_only(repo_id, **kwargs):
        calls.append(kwargs)
        raise LocalEntryNotFoundError("not cached")

    monkeypatch.setenv("ACCIO_CAPTION", "1")
    monkeypatch.setattr(caption, "snapshot_download", offline_only)
    caption._load.cache_clear()
    try:
        start = time.monotonic()
        assert caption.caption_image(png_hallo_file) == ""
        assert caption.caption_image(png_hallo_file) == ""
        assert time.monotonic() - start < 1
        assert calls == [{"local_files_only": True}]  # checked once, never downloads
    finally:
        caption._load.cache_clear()


def test_caption_input_downscaled_to_1024(tmp_path):
    from PIL import Image

    from accio_ingest.caption import caption_input

    big = tmp_path / "big.png"
    Image.new("RGB", (4000, 1200), "white").save(big)
    with caption_input(big) as path:
        assert max(Image.open(path).size) == 1024
    small = tmp_path / "small.png"
    Image.new("RGB", (800, 600), "white").save(small)
    with caption_input(small) as path:
        assert path == small
