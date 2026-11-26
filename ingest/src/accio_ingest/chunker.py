import re

import yaml

from .types import Chunk

FRONTMATTER = re.compile(r"\A---[ \t]*\n(.*?)^---[ \t]*(?:\n|$)", re.M | re.S)
HEADING = re.compile(r"^ {0,3}(#{1,3})[ \t]+(.+?)\s*$")
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,}|\$\$)(.*)$")
LABEL = re.compile(r'\[([^\]]+)\]|\(([^)]+)\)|\{([^}]+)\}|"([^"]+)"|\|([^|]+)\|')
WIKILINK = re.compile(r"(!?)\[\[([^\]\n]+)\]\]")


def mermaid_to_text(src: str) -> str:
    labels = {}
    for match in LABEL.finditer(src):
        label = next(value for value in match.groups() if value is not None)
        label = label.strip(' "`')
        if label:
            labels[label] = None
    return " ".join(filter(None, ("[mermaid diagram]", "; ".join(labels))))


def _wikilink(match: re.Match) -> str:
    if match[1]:
        return ""
    target, separator, alias = match[2].partition("|")
    return alias if separator else target.split("#", 1)[0]


def _split_at(text: str, limit: int) -> int:
    boundary = max(text.rfind(" ", 0, limit + 1), text.rfind("\n", 0, limit + 1))
    return boundary if boundary > 0 else limit


def _split_paragraph(text: str, limit: int):
    while len(text) > limit:
        boundary = _split_at(text, limit)
        yield text[:boundary].rstrip()
        text = text[boundary:].lstrip()
    if text:
        yield text


def _pack(blocks: list[tuple[str, bool]], max_chars: int, min_chars: int):
    packed: list[str] = []
    pending = ""
    for text, atomic in blocks:
        remaining = max_chars - len(pending) - 2
        if not atomic and 0 < len(pending) < min_chars and 0 < remaining < len(text):
            # Fill a short chunk from the next paragraph without splitting fenced blocks.
            boundary = _split_at(text, remaining)
            packed.append(f"{pending}\n\n{text[:boundary].rstrip()}")
            pending, text = "", text[boundary:].lstrip()
        parts = [text] if atomic else _split_paragraph(text, max_chars)
        for part in parts:
            if pending and len(pending) + 2 + len(part) > max_chars:
                packed.append(pending)
                pending = ""
            pending = f"{pending}\n\n{part}" if pending else part
    if pending:
        packed.append(pending)
    return packed


def chunk_markdown(
    md: str,
    title: str,
    page: int | None = None,
    header_prefix: str | None = None,
    max_chars: int = 1800,
    min_chars: int = 200,
) -> list[Chunk]:
    if max_chars <= 0 or min_chars < 0:
        raise ValueError("invalid chunk size")
    md = md.lstrip("\ufeff").replace("\r\n", "\n")
    suffix = ""
    if front := FRONTMATTER.match(md):
        md = md[front.end():]
        try:
            fields = yaml.safe_load(front[1])
        except yaml.YAMLError:
            fields = None
        tags = fields.get("tags") if isinstance(fields, dict) else None
        if isinstance(tags, list):
            tags = ", ".join(map(str, tags))
        if isinstance(tags, str) and tags.strip():
            suffix = f" [tags: {tags.strip()}]"

    root = title if header_prefix is None else header_prefix
    headings: list[tuple[int, str]] = []
    chunks = []
    blocks: list[tuple[str, bool]] = []
    paragraph: list[str] = []
    fence: list[str] = []
    closing = None
    mermaid = False

    def finish_paragraph():
        text = "\n".join(paragraph).strip()
        if text:
            blocks.append((text, False))
        paragraph.clear()

    def finish_section():
        finish_paragraph()
        header = " › ".join([root, *(name for _, name in headings)]) + suffix
        chunks.extend(Chunk(header, text, page) for text in _pack(blocks, max_chars, min_chars))
        blocks.clear()

    for line in md.splitlines():
        if closing is not None:
            fence.append(line)
            if closing.fullmatch(line):
                text = mermaid_to_text("\n".join(fence[1:-1])) if mermaid else "\n".join(fence)
                blocks.append((text, True))
                fence.clear()
                closing = None
            continue
        if start := FENCE.match(line):
            finish_paragraph()
            marker, info = start.groups()
            if marker == "$$" and info.rstrip().endswith("$$"):
                blocks.append((line, True))
                continue
            repeats = f"{{{len(marker)},}}" if marker != "$$" else "{2}"
            closing = re.compile(r" {0,3}" + re.escape(marker[0]) + repeats + r"[ \t]*")
            mermaid = info.strip().lower() == "mermaid" and marker != "$$"
            fence.append(line)
            continue
        line = WIKILINK.sub(_wikilink, line)
        if heading := HEADING.match(line):
            finish_section()
            level = len(heading[1])
            while headings and headings[-1][0] >= level:
                headings.pop()
            headings.append((level, re.sub(r"\s+#+$", "", heading[2])))
        elif line.strip():
            paragraph.append(line)
        else:
            finish_paragraph()
    if fence:
        blocks.append(("\n".join(fence), True))
    finish_section()
    return chunks
