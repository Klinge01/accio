from io import BytesIO
from pathlib import Path
from tempfile import TemporaryDirectory

from .caption import caption_image
from .ocr import ocr_image
from .types import Chunk


def parse_image(path: Path) -> list[Chunk]:
    if path.stat().st_size < 20_000:
        return []

    from PIL import Image, ImageOps

    if path.suffix.lower() == ".heic":
        from pillow_heif import register_heif_opener

        register_heif_opener()
    with Image.open(path) as image:
        if min(image.size) < 256:
            return []
        png = BytesIO()
        ImageOps.exif_transpose(image).convert("RGB").save(png, format="PNG")
    text = ocr_image(png.getvalue())
    if path.suffix.lower() == ".heic":
        with TemporaryDirectory() as temporary:
            normalized = Path(temporary) / "image.png"
            normalized.write_bytes(png.getvalue())
            caption = caption_image(normalized)
    else:
        caption = caption_image(path)
    parts = [label + value for label, value in (("Caption: ", caption), ("Text in image: ", text)) if value]
    return [Chunk(f"{path.name} (image)", "\n\n".join(parts))] if parts else []
