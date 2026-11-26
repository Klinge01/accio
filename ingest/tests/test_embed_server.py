import base64
import io
import json
import struct

import pytest

from accio_ingest.embed_server import serve_embeddings


def fake_embed(texts):
    return [[float(len(t)), 1.0, 2.0] for t in texts]


def run(requests, embed=fake_embed):
    out = io.StringIO()
    serve_embeddings(io.StringIO("".join(json.dumps(r) + "\n" for r in requests)), out, embed, lambda text, to: None)
    return [json.loads(line) for line in out.getvalue().splitlines()]


def test_vectors_roundtrip_as_base64_f32le():
    [resp] = run([{"id": 3, "texts": ["ab", "abcd"]}])
    assert (resp["id"], resp["n"], resp["dim"]) == (3, 2, 3)
    values = struct.unpack("<6f", base64.b64decode(resp["f32le"]))
    assert values == (2.0, 1.0, 2.0, 4.0, 1.0, 2.0)


def test_error_is_reported_and_server_keeps_going():
    def flaky(texts):
        if texts == ["bad"]:
            raise RuntimeError("boom")
        return fake_embed(texts)

    first, second = run([{"id": 1, "texts": ["bad"]}, {"id": 2, "texts": ["ok"]}], flaky)
    assert first == {"id": 1, "error": "RuntimeError: boom"}
    assert second["n"] == 1


@pytest.mark.slow
def test_real_model_semantic_order():
    from accio_ingest.embed_server import load_embedder

    embed = load_embedder()
    q, rel, irr = embed([
        "task: search result | query: Wie hoch ist die Miete?",
        "title: Wohnung › Kosten | text: Die Kaltmiete beträgt 950 Euro im Monat",
        "title: Rezept › Kuchen | text: Mehl und Zucker verrühren",
    ])
    dot = lambda a, b: sum(x * y for x, y in zip(a, b))
    assert dot(q, rel) > dot(q, irr)


def test_translate_request_returns_text():
    calls = []

    def translate(text, to):
        calls.append((text, to))
        return f"{to}:{text}"

    out = io.StringIO()
    requests = [{"id": 5, "translate": "was ist das?", "to": "en"}, {"id": 6, "texts": ["ok"]}]
    serve_embeddings(io.StringIO("".join(json.dumps(r) + "\n" for r in requests)), out, fake_embed, translate)
    first, second = [json.loads(line) for line in out.getvalue().splitlines()]
    assert first == {"id": 5, "text": "en:was ist das?"}
    assert second["n"] == 1
    assert calls == [("was ist das?", "en")]


def test_translate_not_ready_returns_null():
    out = io.StringIO()
    request = {"id": 7, "translate": "hello", "to": "de"}
    serve_embeddings(io.StringIO(json.dumps(request) + "\n"), out, fake_embed, lambda text, to: None)
    assert json.loads(out.getvalue()) == {"id": 7, "text": None}


def test_translator_waits_for_nothing_while_loading():
    from accio_ingest.translate import Translator

    import threading

    release = threading.Event()

    def slow_load(pair):
        release.wait(5)
        return lambda text: f"{pair}:{text}"

    translator = Translator(slow_load)
    assert translator("hello", "de") is None  # still loading: skip, never block a query
    release.set()
    translator.ready.wait(5)
    assert translator("hello", "de") == "en-de:hello"
    assert translator("hallo", "en") == "de-en:hallo"
    assert translator("x", "fr") is None
