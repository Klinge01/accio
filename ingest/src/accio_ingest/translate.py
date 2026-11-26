"""Query translation between German and English for cross-lingual retrieval.

Helsinki-NLP opus-mt (Marian) models, converted for CTranslate2, run int8 on the CPU:
~60-120 ms per question, so translation overlaps the GPU embedding without competing
for it. Models load in the background; until they are ready, translation is skipped
instead of delaying a question.
"""
import os
import threading
from typing import Callable

MODELS = {
    "en-de": ("gaudi/opus-mt-en-de-ctranslate2", "3998844c0d44ff2aa5aaf5a1cf8365b6eb4fd783"),
    "de-en": ("gaudi/opus-mt-de-en-ctranslate2", "7134d80efb6dbc284567cbeb55a517dc2974f722"),
}
TARGET_PAIR = {"de": "en-de", "en": "de-en"}

Translate = Callable[[str], str]


def model_path(pair: str) -> str:
    from huggingface_hub import snapshot_download

    repo, revision = MODELS[pair]
    try:
        return snapshot_download(repo, revision=revision, local_files_only=True)
    except Exception:
        os.environ["HF_HUB_DISABLE_XET"] = "1"  # xet transfers stall on this network
        return snapshot_download(repo, revision=revision)


def load_pair(pair: str) -> Translate:
    import ctranslate2
    import sentencepiece

    path = model_path(pair)
    model = ctranslate2.Translator(path, device="cpu", compute_type="int8", intra_threads=4)
    source = sentencepiece.SentencePieceProcessor(os.path.join(path, "source.spm"))
    target = sentencepiece.SentencePieceProcessor(os.path.join(path, "target.spm"))

    def translate(text: str) -> str:
        tokens = source.encode(text, out_type=str) + ["</s>"]  # Marian needs the EOS token
        result = model.translate_batch([tokens], beam_size=2, max_decoding_length=96)
        return target.decode(result[0].hypotheses[0])

    return translate


class Translator:
    """`translator(text, to)` returns the translation, or None while models load or on failure."""

    def __init__(self, load: Callable[[str], Translate] = load_pair):
        self._pairs: dict[str, Translate] = {}
        self.ready = threading.Event()
        threading.Thread(target=self._load_all, args=(load,), daemon=True).start()

    def _load_all(self, load: Callable[[str], Translate]) -> None:
        for pair in MODELS:
            try:
                self._pairs[pair] = load(pair)
            except Exception as error:  # translation is optional; retrieval still works
                print(f"translation model {pair} unavailable: {error}", flush=True)
        self.ready.set()

    def __call__(self, text: str, to: str) -> str | None:
        pair = self._pairs.get(TARGET_PAIR.get(to, ""))
        return pair(text) if pair else None


def fetch_models() -> list[str]:
    return [model_path(pair) for pair in MODELS]
