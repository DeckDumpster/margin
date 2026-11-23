ALTER TABLE comments ADD COLUMN visibility TEXT NOT NULL DEFAULT 'now'
    CHECK (visibility IN ('now', 'held'));

-- One row per write, inserted in the write's own transaction. `author` and `visibility` are the
-- comment's at the time of the write, so the feed can still say who wrote a comment that has
-- since been deleted, and the page feed can still tell a held comment's rows from its own.
CREATE TABLE changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN (
        'thread_created', 'comment_created', 'comment_updated', 'comment_deleted',
        'published', 'collection_updated')),
    thread_id TEXT,
    comment_id TEXT,
    author TEXT,
    visibility TEXT CHECK (visibility IN ('now', 'held')),
    at INTEGER NOT NULL
);
CREATE INDEX changes_by_collection ON changes (collection_id, seq);
