CREATE TABLE anchors (
    id INTEGER PRIMARY KEY,
    version_id INTEGER NOT NULL REFERENCES versions (id) ON DELETE CASCADE,
    first_line INTEGER NOT NULL,
    last_line INTEGER NOT NULL,
    start_col INTEGER,
    end_col INTEGER,
    CHECK (first_line <= last_line),
    CHECK ((start_col IS NULL) = (end_col IS NULL))
);
CREATE INDEX anchors_by_version ON anchors (version_id);

CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    collection_id TEXT NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    anchor_id INTEGER NOT NULL REFERENCES anchors (id),
    created_at INTEGER NOT NULL
);
CREATE INDEX threads_by_collection ON threads (collection_id, created_at, id);

CREATE TABLE comments (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX comments_by_thread ON comments (thread_id, created_at, id);
