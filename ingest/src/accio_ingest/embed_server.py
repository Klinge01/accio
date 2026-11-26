"""Resident embedding helper for the Rust app: EmbeddingGemma on the Apple GPU (MLX).

Protocol (JSON lines on stdin/stdout):
  request  {"id": int, "texts": [str, ...]}           (prompt prefixes added by Rust)
  response {"id": int, "n": int, "dim": int, "f32le": base64}  row-major little-endian f32
  request  {"id": int, "translate": str, "to": "de" | "en"}
  response {"id": int, "text": str | null}            null: translation not available yet
  any request may instead get {"id": int, "error": str}
Exits on EOF, i.e. when the app quits.
"""
import base64
import json
import os
import struct
import sys
from typing import Callable, TextIO

MODEL = "mlx-community/embeddinggemma-300m-bf16"
MAX_TOKENS = 512
CACHE_LIMIT_BYTES = 256 * 1024 * 1024

Embed = Callable[[list[str]], list[list[float]]]
Translate = Callable[[str, str], str | None]


def model_path() -> str:
    from huggingface_hub import snapshot_download

    try:
        return snapshot_download(MODEL, local_files_only=True)
    except Exception:
        os.environ["HF_HUB_DISABLE_XET"] = "1"  # xet transfers stall on this network
        return snapshot_download(MODEL)


def load_embedder() -> Embed:
    import mlx.core as mx
    from mlx_embeddings.utils import load

    # MLX keeps freed GPU buffers for reuse without bound. Every batch has a different
    # padded shape, so on a long ingestion the cache grew to 25 GB (a large monorepo, ~20k chunks).
    mx.set_cache_limit(CACHE_LIMIT_BYTES)
    model, tokenizer = load(model_path())
    # fp16 is ~16% faster than the shipped bf16 on Apple GPUs; vectors are identical
    # (measured cosine 1.0000 on 128 real chunks), so existing indexes stay valid.
    model.set_dtype(mx.float16)

    def embed(texts: list[str]) -> list[list[float]]:
        batch = tokenizer._tokenizer(
            texts, padding=True, truncation=True, max_length=MAX_TOKENS, return_tensors="mlx"
        )
        vectors = model(batch["input_ids"], attention_mask=batch["attention_mask"]).text_embeds
        mx.eval(vectors)
        return vectors.astype(mx.float32).tolist()

    return embed


def serve_embeddings(
    stdin: TextIO, stdout: TextIO, embed: Embed | None = None, translate: Translate | None = None
) -> None:
    if translate is None:
        from .translate import Translator

        translate = Translator()  # loads in the background
    embed = embed or load_embedder()
    for line in stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        try:
            if "translate" in request:
                response = {"id": request["id"], "text": translate(request["translate"], request["to"])}
                stdout.write(json.dumps(response) + "\n")
                stdout.flush()
                continue
            vectors = embed(request["texts"])
            dim = len(vectors[0]) if vectors else 0
            flat = [value for vector in vectors for value in vector]
            payload = base64.b64encode(struct.pack(f"<{len(flat)}f", *flat)).decode()
            response = {"id": request["id"], "n": len(vectors), "dim": dim, "f32le": payload}
        except Exception as error:  # report and keep serving
            response = {"id": request["id"], "error": f"{type(error).__name__}: {error}"}
        stdout.write(json.dumps(response) + "\n")
        stdout.flush()


if __name__ == "__main__":
    serve_embeddings(sys.stdin, sys.stdout)
