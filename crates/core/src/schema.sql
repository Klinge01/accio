CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY, value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    ignore_globs TEXT NOT NULL,
    last_ingested_at INTEGER,
    status TEXT NOT NULL DEFAULT 'idle',
    pdf_mode TEXT NOT NULL DEFAULT 'text',
    enabled INTEGER NOT NULL DEFAULT 1,
    is_file INTEGER NOT NULL DEFAULT 0,
    captions INTEGER,
    kind TEXT NOT NULL DEFAULT 'notes'
);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    path TEXT NOT NULL UNIQUE,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL,
    hash TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    ingested_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS source_volumes (
    source_id INTEGER PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
    identity TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS files_source ON files(source_id);
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    header TEXT NOT NULL,
    text TEXT NOT NULL,
    page INTEGER,
    vec BLOB NOT NULL,
    line INTEGER,
    end_line INTEGER
);
CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    header, text, content='chunks', content_rowid='id', tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, header, text) VALUES (new.id, new.header, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, header, text) VALUES ('delete', old.id, old.header, old.text);
END;

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY, value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE COLLATE NOCASE
);
CREATE TABLE IF NOT EXISTS profile_sources (
    profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    PRIMARY KEY(profile_id, source_id)
);
