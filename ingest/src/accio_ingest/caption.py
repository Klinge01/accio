import os
import sys
import tempfile
from contextlib import contextmanager
from functools import cache
from pathlib import Path

from huggingface_hub import snapshot_download

MODEL = "mlx-community/Qwen2.5-VL-3B-Instruct-4bit"
PROMPT = "Describe this image in 2-3 sentences. Mention any diagram, chart or UI and what it shows."
MAX_SIDE = 1024


def fetch_model() -> str:
    """Download the caption model. Plain HTTP: hf_xet transfers stall on this network."""
    os.environ["HF_HUB_DISABLE_XET"] = "1"
    return snapshot_download(MODEL)


@cache
def _load():
    """Load from the local cache only. Never download inside the ingestion worker:
    a download there runs into the per-file timeout and restarts forever."""
    try:
        path = snapshot_download(MODEL, local_files_only=True)
    except Exception:
        print(
            f"caption model {MODEL} not downloaded; images get OCR text only. "
            "Run: uv run accio-ingest fetch-models",
            file=sys.stderr,
        )
        return None
    from mlx_vlm import load

    return load(path)


@contextmanager
def caption_input(path: Path):
    """Yield a path to an image whose longest side is at most MAX_SIDE pixels.
    Vision-token count grows with pixel count; captions don't need full resolution."""
    from PIL import Image

    with Image.open(path) as image:
        if max(image.size) <= MAX_SIDE:
            yield path
            return
        image = image.convert("RGB")
        image.thumbnail((MAX_SIDE, MAX_SIDE))
        with tempfile.NamedTemporaryFile(suffix=".png") as scaled:
            image.save(scaled.name)
            yield Path(scaled.name)


def caption_image(path: Path) -> str:
    # Opt-in: ~11-27 s per image on an M4 vs ~0.2 s for OCR.
    if os.getenv("ACCIO_CAPTION") != "1":
        return ""
    loaded = _load()
    if loaded is None:
        return ""

    from mlx_vlm import generate
    from mlx_vlm.prompt_utils import apply_chat_template

    model, processor = loaded
    prompt = apply_chat_template(processor, model.config, PROMPT, num_images=1)
    with caption_input(path) as image:
        result = generate(model, processor, prompt, image=[str(image)], max_tokens=120, verbose=False)
    return result.text.strip()
