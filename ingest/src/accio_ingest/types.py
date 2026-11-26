from dataclasses import asdict, dataclass


@dataclass
class Chunk:
    header: str
    text: str
    page: int | None = None
    # Code chunks: first and last line in the file (1-based).
    line: int | None = None
    end_line: int | None = None

    def to_json(self) -> dict:
        return asdict(self)
