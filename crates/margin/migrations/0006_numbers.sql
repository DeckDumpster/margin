-- A review's short number. AUTOINCREMENT, so a deleted review's number is never given again.
CREATE TABLE collection_numbers (
    number        INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id TEXT NOT NULL UNIQUE REFERENCES collections (id) ON DELETE CASCADE
);
INSERT INTO collection_numbers (collection_id)
    SELECT id FROM collections ORDER BY created_at, id;
