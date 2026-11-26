from accio_ingest.chunker import chunk_markdown


def test_frontmatter_tags_and_wikilinks():
    [chunk] = chunk_markdown(
        "---\ntags: [ai, rag]\n---\n# Intro\n"
        "See [[Other#Sec|that note]], [[Plain#H]] and [[Note]] ![[img.png]]",
        "Note",
    )
    assert chunk.header == "Note › Intro [tags: ai, rag]"
    assert chunk.text == "See that note, Plain and Note"


def test_headings_and_small_chunks():
    markdown = "# A\nshort\n## B\nalso short\n#### C\n" + "\n\n".join(["tiny"] * 5)
    chunks = chunk_markdown(markdown, "N")
    assert [c.header for c in chunks] == ["N › A", "N › A › B"]
    assert "#### C" in chunks[1].text


def test_code_and_math_atomic():
    code = "```python\n" + "\n\n".join(["x = '[[literal]]'"] * 600) + "\n```"
    math = "$$\na\n\nb\n$$"
    chunks = chunk_markdown("# S\n" + code + "\n\n" + "p " * 950 + "\n\n" + math, "N")
    assert chunks[0].text == code
    assert sum(math in chunk.text for chunk in chunks) == 1


def test_mermaid_replaced():
    [chunk] = chunk_markdown("```mermaid\ngraph TD\nA[Start] -->|go| B(End)\n```", "N")
    assert chunk.text == "[mermaid diagram] Start; go; End"


def test_long_section_split_max_chars():
    markdown = "\n\n".join(["word " * 60] * 20) + "\n\n" + "a" * 1900
    chunks = chunk_markdown(markdown, "N")
    assert len(chunks) > 1 and all(len(c.text) <= 1800 for c in chunks)

def test_short_paragraph_merges_into_the_next_chunk():
    chunks = chunk_markdown("Brief intro\n\n" + "plain text " * 220, "N")
    assert chunks[0].text.startswith("Brief intro\n\nplain text")
    assert 200 <= len(chunks[0].text) <= 1800


def test_header_prefix_and_page():
    [chunk] = chunk_markdown("text", "x", page=3, header_prefix="f.pdf › p.3")
    assert (chunk.header, chunk.page) == ("f.pdf › p.3", 3)


def test_empty_returns_no_chunks():
    assert chunk_markdown("---\na: 1\n---\n   \n", "N") == []
