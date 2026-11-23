-- Where a review's documents came from: a repository's web URL and a commit, both or neither.
ALTER TABLE collections ADD COLUMN source_repository TEXT;
ALTER TABLE collections ADD COLUMN source_commit TEXT
    CHECK ((source_commit IS NULL) = (source_repository IS NULL));
