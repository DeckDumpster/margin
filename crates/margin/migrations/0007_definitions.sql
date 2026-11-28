-- What each version defines, recorded the first time it is parsed, so a reader page parses only
-- the documents it shows. A version is unrecorded until then, which an empty list is not.
ALTER TABLE versions ADD COLUMN definitions_recorded INTEGER NOT NULL DEFAULT 0
    CHECK (definitions_recorded IN (0, 1));

CREATE TABLE definitions (
    version_id INTEGER NOT NULL REFERENCES versions (id) ON DELETE CASCADE,
    -- The order the parse found them in.
    position INTEGER NOT NULL,
    name TEXT NOT NULL,
    line INTEGER NOT NULL CHECK (line >= 1),
    imp INTEGER NOT NULL CHECK (imp IN (0, 1)),
    owner TEXT,
    PRIMARY KEY (version_id, position)
);
