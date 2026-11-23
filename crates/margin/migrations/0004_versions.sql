-- How a thread's anchor reached its version: where its lines survived (placed), where they are
-- gone and it sits at the nearest surviving line (removed), or where a highlight's words changed
-- and it fell back to its line (text_changed).
ALTER TABLE anchors ADD COLUMN carry_state TEXT NOT NULL DEFAULT 'placed'
    CHECK (carry_state IN ('placed', 'removed', 'text_changed'));
-- The anchor this one was carried from, so deleting the newest version can put threads back.
ALTER TABLE anchors ADD COLUMN carried_from INTEGER REFERENCES anchors (id);

-- SQLite cannot widen a CHECK in place: rebuild `changes` with `version_created` allowed.
CREATE TABLE changes_new (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN (
        'thread_created', 'comment_created', 'comment_updated', 'comment_deleted',
        'published', 'collection_updated', 'version_created')),
    thread_id TEXT,
    comment_id TEXT,
    author TEXT,
    visibility TEXT CHECK (visibility IN ('now', 'held')),
    at INTEGER NOT NULL
);
INSERT INTO changes_new SELECT seq, collection_id, kind, thread_id, comment_id, author, visibility, at FROM changes;
DROP TABLE changes;
ALTER TABLE changes_new RENAME TO changes;
CREATE INDEX changes_by_collection ON changes (collection_id, seq);
