"""Chunking for code repositories.

Chunks follow the code's structure (tree-sitter), not a fixed size:

- Java: one outline per type (annotations, extends/implements, fields, enum constants,
  method signatures), one chunk per non-trivial method or constructor.
- JavaScript/TypeScript/Vue/Python: top-level declarations grouped up to the chunk size;
  oversized ones split along their own structure (class members, object properties,
  statements). A module outline lists imports and declarations.
- SQL: statements grouped, with the tables they create or change in the header.
- Spring config (YAML/properties): grouped by top-level key.

Every chunk except outlines is a verbatim slice of the file, with its first and last
line, so agents get exact file:line pointers. Headers carry the qualified symbol plus
its name split into words ("LeadSourceType" -> "lead source type") so keyword search
finds identifiers from plain-language questions.
"""
import re
from collections.abc import Callable
from functools import cache
from pathlib import Path

from .chunker import chunk_markdown
from .types import Chunk

MAX_CHARS = 1500
MAX_TEMPLATE_PARTS = 2
CODE_SUFFIXES = {".java", ".js", ".mjs", ".cjs", ".jsx", ".ts", ".tsx", ".vue", ".py", ".sql"}
CONFIG_SUFFIXES = {".yml", ".yaml", ".properties"}

JAVA_TYPES = {
    "class_declaration", "interface_declaration", "enum_declaration", "record_declaration",
    "annotation_type_declaration",
}
JAVA_CALLABLES = {
    "method_declaration", "constructor_declaration", "compact_constructor_declaration",
}
JAVA_KIND = {
    "class_declaration": "class", "interface_declaration": "interface",
    "enum_declaration": "enum", "record_declaration": "record",
    "annotation_type_declaration": "annotation",
}
COMMENTS = {"comment", "line_comment", "block_comment"}


@cache
def _parser(language: str):
    from tree_sitter import Language, Parser

    if language == "java":
        import tree_sitter_java as grammar

        handle = grammar.language()
    elif language == "python":
        import tree_sitter_python as grammar

        handle = grammar.language()
    elif language in ("typescript", "tsx"):
        import tree_sitter_typescript as grammar

        handle = grammar.language_tsx() if language == "tsx" else grammar.language_typescript()
    else:
        import tree_sitter_javascript as grammar

        handle = grammar.language()
    return Parser(Language(handle))


def split_words(*names: str) -> str:
    """Identifier names as lowercase words: "LeadSourceType", "send_sms" -> "lead source type send sms"."""
    words: list[str] = []
    for name in names:
        for part in re.findall(r"[A-Z]+(?=[A-Z][a-z]|\d|\b|_)|[A-Z]?[a-z]+|[A-Z]+|\d+", name or ""):
            word = part.lower()
            if word not in words:
                words.append(word)
    return " ".join(words)


def _collapse(text: str, limit: int = 200) -> str:
    text = re.sub(r"\s+", " ", text).strip()
    return text if len(text) <= limit else text[: limit - 1] + "…"


class Source:
    """A file's text with byte/line helpers for tree-sitter nodes."""

    def __init__(self, text: str, row_offset: int = 0):
        self.text = text
        self.bytes = text.encode("utf-8")
        self.lines = text.split("\n")
        self.row_offset = row_offset

    def of(self, node) -> str:
        return self.bytes[node.start_byte:node.end_byte].decode("utf-8", "replace")

    def between(self, start: int, end: int) -> str:
        return self.bytes[start:end].decode("utf-8", "replace")

    def span_chars(self, first_row: int, last_row: int) -> int:
        return sum(len(line) + 1 for line in self.lines[first_row:last_row + 1])

    def chunks(self, header: str, first_row: int, last_row: int) -> list[Chunk]:
        """Verbatim chunks for rows first..last, split at line boundaries to fit MAX_CHARS.
        Blank lines at either end are dropped so line numbers point at code."""
        result = []
        start = first_row
        while start <= last_row:
            end, size = start, 0
            while end <= last_row and (end == start or size + len(self.lines[end]) + 1 <= MAX_CHARS):
                size += len(self.lines[end]) + 1
                end += 1
            low, high = start, end - 1
            while low <= high and not self.lines[low].strip():
                low += 1
            while high >= low and not self.lines[high].strip():
                high -= 1
            if low <= high:
                text = "\n".join(self.lines[low:high + 1])[: MAX_CHARS * 2]  # one huge line
                result.append(Chunk(
                    header, text, None, low + 1 + self.row_offset, high + 1 + self.row_offset
                ))
            start = end
        return result


def _with_words(header: str, *names: str) -> str:
    words = split_words(*names)
    return f"{header} — {words}" if words else header


# --- Java -----------------------------------------------------------------------


def _javadoc_start(node, source: Source) -> int:
    """First row of the member including a Javadoc right above it."""
    previous = node.prev_sibling
    if (
        previous is not None
        and previous.type == "block_comment"
        and source.of(previous).startswith("/**")
        and node.start_point[0] - previous.end_point[0] <= 1
    ):
        return previous.start_point[0]
    return node.start_point[0]


def _annotations(node, source: Source) -> list[str]:
    for child in node.children:
        if child.type == "modifiers":
            return [
                _collapse(source.of(part), 60)
                for part in child.named_children
                if part.type in ("marker_annotation", "annotation")
            ]
    return []


def _signature(node, source: Source) -> str:
    body = node.child_by_field_name("body")
    end = body.start_byte if body is not None else node.end_byte
    text = source.between(node.start_byte, end).rstrip().rstrip(";")
    # Drop annotations from the signature; they are listed separately.
    text = re.sub(r"@[\w.]+(\([^()]*(\([^()]*\)[^()]*)*\))?\s*", "", text)
    return _collapse(text, 220)


def _trivial(node, source: Source) -> bool:
    """Accessors and one-line delegations: their signature in the outline is enough."""
    body = node.child_by_field_name("body")
    if body is None:  # abstract or interface method: keep only when documented
        start = _javadoc_start(node, source)
        return start == node.start_point[0] or source.span_chars(start, node.start_point[0]) < 80
    statements = [child for child in body.named_children if child.type not in COMMENTS]
    return len(statements) <= 1 and len(source.of(body)) <= 120


def _java_type(node, qualifier: str, package: str, source: Source, out: list[Chunk], top: bool = True):
    name_node = node.child_by_field_name("name")
    name = source.of(name_node) if name_node is not None else "?"
    qname = f"{qualifier}.{name}" if qualifier else name
    kind = JAVA_KIND.get(node.type, "type")
    relations = [
        _collapse(source.of(child), 120)
        for child in node.children
        if child.type in ("superclass", "super_interfaces", "extends_interfaces")
    ]
    info = " ".join(_annotations(node, source) + relations)
    info = f" · {_collapse(info, 160)}" if info else ""
    body = node.child_by_field_name("body")

    outline = [f"package {package};"] if package and top else []
    outline.append(_collapse(
        source.between(node.start_byte, body.start_byte if body is not None else node.end_byte), 300
    ) + " {")
    constants: list[str] = []
    nested = []
    members = list(body.named_children) if body is not None else []
    index = 0
    while index < len(members):
        member = members[index]
        index += 1
        if member.type == "enum_body_declarations":
            members[index:index] = list(member.named_children)
        elif member.type == "enum_constant":
            constants.append(_collapse(source.of(member).split("{", 1)[0], 100))
        elif member.type in ("field_declaration", "constant_declaration"):
            outline.append("  " + _collapse(source.of(member), 200))
        elif member.type == "annotation_type_element_declaration":
            outline.append("  " + _signature(member, source) + ";")
        elif member.type in JAVA_CALLABLES:
            signature = _signature(member, source)
            outline.append(f"  {signature};")
            if not _trivial(member, source):
                method = member.child_by_field_name("name")
                method_name = source.of(method) if method is not None else name
                header = _with_words(f"{qname} › {signature}{info}", name, method_name)
                out.extend(source.chunks(header, _javadoc_start(member, source), member.end_point[0]))
        elif member.type == "static_initializer":
            outline.append("  static { … }")
            out.extend(source.chunks(f"{qname} › static initializer{info}", member.start_point[0], member.end_point[0]))
        elif member.type in JAVA_TYPES:
            nested_name = member.child_by_field_name("name")
            label = source.of(nested_name) if nested_name is not None else "?"
            outline.append(f"  {JAVA_KIND.get(member.type, 'type')} {label} {{ … }}")
            nested.append(member)
    if constants:
        declaration_at = 2 if outline[0].startswith("package ") else 1
        outline.insert(declaration_at, "  " + ", ".join(constants) + ";")
    outline.append("}")
    header = _with_words(f"{qname} ({kind}){info}", name)
    # Outlines are synthesized, not verbatim: the line range covers the whole type.
    first = node.start_point[0] + source.row_offset + 1
    last = node.end_point[0] + source.row_offset + 1
    for part in _pack_lines(outline, MAX_CHARS):
        out.append(Chunk(header, part, None, first, last))
    for member in nested:
        _java_type(member, qname, package, source, out, top=False)


def _pack_lines(lines: list[str], limit: int) -> list[str]:
    parts, current = [], ""
    for line in lines:
        if current and len(current) + 1 + len(line) > limit:
            parts.append(current)
            current = ""
        current = f"{current}\n{line}" if current else line[: limit * 2]
    if current:
        parts.append(current)
    return parts


def chunk_java(text: str, label: str) -> list[Chunk]:
    source = Source(text)
    root = _parser("java").parse(source.bytes).root_node
    package = ""
    for child in root.named_children:
        if child.type == "package_declaration":
            package = re.sub(r"^package\s+|;$", "", _collapse(source.of(child))).strip()
    chunks: list[Chunk] = []
    for child in root.named_children:
        if child.type in JAVA_TYPES:
            _java_type(child, package, package, source, chunks)
    return chunks or source.chunks(label, 0, len(source.lines) - 1)


# --- JavaScript / TypeScript / Python (structure-following) ---------------------


def _size(node) -> int:
    return node.end_byte - node.start_byte


def _name(node, source: Source) -> str | None:
    """A readable name for a declaration-like node, if it has one."""
    named = node.child_by_field_name("name")
    if named is not None:
        return _collapse(source.of(named), 60)
    if node.type == "export_statement":
        declaration = node.child_by_field_name("declaration")
        if declaration is not None:
            return _name(declaration, source)
        return "default export" if "default" in source.of(node)[:20] else None
    if node.type == "decorated_definition":
        definition = node.child_by_field_name("definition")
        return _name(definition, source) if definition is not None else None
    if node.type in ("lexical_declaration", "variable_declaration"):
        for child in node.named_children:
            if child.type == "variable_declarator":
                return _name(child, source)
    if node.type == "pair":
        key = node.child_by_field_name("key")
        return _collapse(source.of(key), 60) if key is not None else None
    if node.type in ("expression_statement", "call_expression"):
        call = node if node.type == "call_expression" else (node.named_children or [None])[0]
        if call is not None and call.type == "call_expression":
            function = call.child_by_field_name("function")
            if function is not None:
                return _collapse(source.of(function), 60)
    return None


def _container(node):
    """The node whose children carry `node`'s content: descend through wrappers
    (declaration -> body, call -> arguments) while one child holds 90% of it."""
    current = node
    for _ in range(6):
        children = [child for child in current.named_children if child.type not in COMMENTS]
        if not children:
            break
        biggest = max(children, key=_size)
        if _size(biggest) < 0.9 * _size(current):
            break
        current = biggest
    return current if current.named_children else None


def _structured(
    container, source: Source, label: str, path: list[str], out: list[Chunk], lead_row: int | None = None
):
    """Groups `container`'s children into chunks up to MAX_CHARS; an oversized child is
    split along its own structure. `lead_row` pulls the parent's opening lines (its
    signature) into the first chunk."""
    group: list = []
    group_start: int | None = lead_row

    def flush():
        nonlocal group, group_start
        if group:
            first = group_start if group_start is not None else group[0].start_point[0]
            names = [name for name in (_name(node, source) for node in group) if name]
            header = " › ".join([label, *path])
            if names:
                header += " › " + ", ".join(names[:4]) + (" …" if len(names) > 4 else "")
            out.extend(source.chunks(_with_words(header, *path, *names[:4]), first, group[-1].end_point[0]))
        group, group_start = [], None

    for child in container.named_children:
        start = child.start_point[0]
        if source.span_chars(start, child.end_point[0]) > MAX_CHARS:
            if all(node.type in COMMENTS for node in group):
                # Comments (and the parent's signature) above stay with the declaration.
                leading = group_start if group_start is not None else (
                    group[0].start_point[0] if group else start)
                group = []
            else:
                flush()
                leading = start
            group_start = None
            inner = _container(child)
            name = _name(child, source) or child.type
            if inner is not None and (inner is not child or len(child.named_children) > 1):
                _structured(inner, source, label, [*path, name], out, lead_row=leading)
            else:
                out.extend(source.chunks(
                    _with_words(" › ".join([label, *path, name]), *path, name), leading, child.end_point[0]
                ))
            continue
        first = group_start if group_start is not None else (group[0].start_point[0] if group else start)
        if group and source.span_chars(first, child.end_point[0]) > MAX_CHARS:
            flush()
        if not group and group_start is None:
            group_start = start
        group.append(child)
    flush()


def _outline(root, source: Source, label: str) -> Chunk | None:
    imports, names = [], []
    for child in root.named_children:
        if child.type in ("import_statement", "import_from_statement"):
            imports.append(_collapse(source.of(child), 160))
        elif child.type not in COMMENTS:
            name = _name(child, source)
            if name and name not in names:
                names.append(name)
    if not imports and not names:
        return None
    lines = imports + ([f"declares: {', '.join(names)}"] if names else [])
    text = "\n".join(_pack_lines(lines, MAX_CHARS)[:1])
    first = source.row_offset + 1
    return Chunk(f"{label} (module outline)", text, None, first, first + len(source.lines) - 1)


def chunk_structured(text: str, label: str, language: str, row_offset: int = 0) -> list[Chunk]:
    source = Source(text, row_offset)
    root = _parser(language).parse(source.bytes).root_node
    chunks: list[Chunk] = []
    _structured(root, source, label, [], chunks)
    outline = _outline(root, source, label)
    if outline is not None and len(chunks) > 1:
        chunks.insert(0, outline)
    return chunks


# --- Vue single-file components ---------------------------------------------------

SCRIPT = re.compile(r"<script\b([^>]*)>(.*?)</script>", re.S | re.I)
TEMPLATE = re.compile(r"<template\b[^>]*>(.*)</template>", re.S | re.I)


def chunk_vue(text: str, label: str) -> list[Chunk]:
    chunks: list[Chunk] = []
    for match in SCRIPT.finditer(text):
        attributes, body = match.group(1), match.group(2)
        language = "typescript" if re.search(r"lang\s*=\s*[\"']tsx?[\"']", attributes) else "javascript"
        offset = text.count("\n", 0, match.start(2))
        setup = " (setup)" if "setup" in attributes else ""
        chunks.extend(chunk_structured(body, f"{label}{setup}", language, offset))
    template = TEMPLATE.search(text)
    if template:
        source = Source(text)
        first = text.count("\n", 0, template.start(1))
        last = text.count("\n", 0, template.end(1))
        # Content rows only, without the <template> and </template> tag lines.
        if text[template.start(1):template.start(1) + 1] == "\n":
            first += 1
        if text[:template.end(1)].endswith("\n"):
            last -= 1
        component = Path(label).stem
        parts = source.chunks(_with_words(f"{label} › template", component), first, last)
        chunks.extend(parts[:MAX_TEMPLATE_PARTS])
    return chunks


# --- SQL and configuration ----------------------------------------------------------

TABLE = re.compile(
    r"\b(?:create|alter|drop)\s+(?:or\s+replace\s+)?(?:table|view|type|index|sequence|function)\s+"
    r"(?:if\s+(?:not\s+)?exists\s+)?(?:\w+\s+on\s+)?([\w.\"]+)",
    re.I,
)


def _grouped(source: Source, label: str, blocks: list[tuple[int, int]], describe: Callable[[str], str]):
    chunks: list[Chunk] = []
    current: tuple[int, int] | None = None
    for start, end in blocks:
        if current and source.span_chars(current[0], end) > MAX_CHARS:
            body = "\n".join(source.lines[current[0]:current[1] + 1])
            chunks.extend(source.chunks(f"{label} › {describe(body)}".rstrip(" ›"), *current))
            current = None
        current = (current[0], end) if current else (start, end)
    if current:
        body = "\n".join(source.lines[current[0]:current[1] + 1])
        chunks.extend(source.chunks(f"{label} › {describe(body)}".rstrip(" ›"), *current))
    return chunks


def chunk_sql(text: str, label: str) -> list[Chunk]:
    source = Source(text)
    blocks, start = [], 0
    for row, line in enumerate(source.lines):
        if line.rstrip().endswith(";"):
            blocks.append((start, row))
            start = row + 1
    if start < len(source.lines):
        blocks.append((start, len(source.lines) - 1))

    def tables(body: str) -> str:
        names = []
        for name in TABLE.findall(body):
            name = name.strip('"')
            if name not in names:
                names.append(name)
        return ", ".join(names[:6])

    return _grouped(source, label, blocks, tables)


def chunk_config(text: str, label: str, properties: bool) -> list[Chunk]:
    source = Source(text)
    blocks, start, keys = [], 0, []
    for row, line in enumerate(source.lines):
        if row == 0 or not line.strip() or line.lstrip().startswith(("#", "!")):
            continue
        top = not line[0].isspace()
        if properties:
            key = re.split(r"[.=:\s]", line.strip(), maxsplit=1)[0]
            top = key != (re.split(r"[.=:\s]", source.lines[start].strip(), maxsplit=1)[0] if source.lines[start].strip() else "")
        if top and line.strip() != "---":
            blocks.append((start, row - 1))
            start = row
    blocks.append((start, len(source.lines) - 1))

    def top_keys(body: str) -> str:
        found = []
        for line in body.split("\n"):
            if line and not line[0].isspace() and not line.startswith(("#", "!", "---")):
                key = re.split(r"[.=:\s]", line.strip(), maxsplit=1)[0]
                if key and key not in found:
                    found.append(key)
        return ", ".join(found[:6])

    return _grouped(source, label, blocks, top_keys)


# --- Entry point --------------------------------------------------------------------


def parse_code(path: Path, root: Path) -> list[Chunk]:
    """Chunks for a file of the code repository at `root`."""
    try:
        label = str(path.resolve().relative_to(root.resolve()))
    except ValueError:
        label = path.name
    text = path.read_text(encoding="utf-8", errors="replace").replace("\r\n", "\n").lstrip("﻿")
    if not text.strip():
        return []
    suffix = path.suffix.lower()
    if suffix == ".md":
        return chunk_markdown(text, label)
    if suffix in CONFIG_SUFFIXES:
        return chunk_config(text, label, properties=suffix == ".properties")
    if suffix == ".sql":
        return chunk_sql(text, label)
    try:
        if suffix == ".java":
            return chunk_java(text, label)
        if suffix == ".vue":
            return chunk_vue(text, label)
        if suffix == ".py":
            return chunk_structured(text, label, "python")
        if suffix in (".ts", ".tsx"):
            return chunk_structured(text, label, "tsx" if suffix == ".tsx" else "typescript")
        if suffix in (".js", ".mjs", ".cjs", ".jsx"):
            return chunk_structured(text, label, "javascript")
    except RecursionError:
        pass
    return Source(text).chunks(label, 0, len(text.split("\n")) - 1)
