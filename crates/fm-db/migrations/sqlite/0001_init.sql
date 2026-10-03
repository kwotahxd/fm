CREATE TABLE categories (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    description TEXT,
    auto        INTEGER NOT NULL DEFAULT 1,      -- 1 = created by the AI, 0 = by the user
    created_at  INTEGER NOT NULL
);

CREATE TABLE files (
    id                INTEGER PRIMARY KEY,
    path              TEXT    NOT NULL UNIQUE,
    name              TEXT    NOT NULL,
    dir               TEXT    NOT NULL,
    size              INTEGER NOT NULL,
    mtime_ns          INTEGER NOT NULL,
    mode              INTEGER NOT NULL,
    mime              TEXT,
    hash              TEXT,                        -- blake3, computed lazily at analysis time
    category_id       INTEGER REFERENCES categories(id) ON DELETE SET NULL,
    summary           TEXT,
    attrs_json        TEXT,                        -- AI-extracted attributes requested by sort rules
    analyzed_at       INTEGER,
    analyzed_mtime_ns INTEGER,                     -- file mtime when analysed (staleness check)
    indexed_at        INTEGER NOT NULL,
    seen_run          INTEGER NOT NULL DEFAULT 0   -- index run that last saw the file (prune deleted files)
);
CREATE INDEX idx_files_dir      ON files(dir);
CREATE INDEX idx_files_hash     ON files(hash);
CREATE INDEX idx_files_category ON files(category_id);
CREATE INDEX idx_files_mime     ON files(mime);

CREATE TABLE tags (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE file_tags (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    tag_id  INTEGER NOT NULL REFERENCES tags(id)  ON DELETE CASCADE,
    PRIMARY KEY (file_id, tag_id)
);
CREATE INDEX idx_file_tags_tag ON file_tags(tag_id);

CREATE TABLE embeddings (
    file_id      INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    model        TEXT    NOT NULL,
    dim          INTEGER NOT NULL,
    vector       BLOB    NOT NULL,                 -- f32 little-endian, L2-normalised
    content_hash TEXT,
    created_at   INTEGER NOT NULL
);

CREATE TABLE sort_rules (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    description TEXT    NOT NULL DEFAULT '',
    prompt      TEXT,                              -- the natural-language prompt it was parsed from
    spec_json   TEXT    NOT NULL,                  -- serialized RuleSet
    source      TEXT    NOT NULL DEFAULT 'user',   -- user | ai
    created_at  INTEGER NOT NULL
);

CREATE TABLE sort_history (
    id         INTEGER PRIMARY KEY,
    batch_id   TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    rule_name  TEXT,
    prompt     TEXT,
    root       TEXT    NOT NULL,
    src        TEXT    NOT NULL,
    dst        TEXT    NOT NULL,
    status     TEXT    NOT NULL CHECK (status IN ('applied','undone','failed')),
    error      TEXT,
    applied_at INTEGER NOT NULL,
    undone_at  INTEGER
);
CREATE INDEX idx_sort_history_batch ON sort_history(batch_id, seq);
