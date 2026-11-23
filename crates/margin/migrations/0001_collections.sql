CREATE TABLE collections (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    cover_note TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('open', 'published', 'closed')),
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX collections_by_age ON collections (created_at DESC, id DESC);

CREATE TABLE documents (
    id TEXT PRIMARY KEY,
    collection_id TEXT NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('code', 'prose')),
    UNIQUE (collection_id, path),
    UNIQUE (collection_id, position)
);

CREATE TABLE versions (
    id INTEGER PRIMARY KEY,
    document_id TEXT NOT NULL REFERENCES documents (id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (document_id, number)
);
