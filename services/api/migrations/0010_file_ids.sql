-- Every stored file ID ever used, so an expired or deleted ID is never reused.
CREATE TABLE file_ids (id TEXT NOT NULL PRIMARY KEY);
INSERT INTO file_ids (id) SELECT id FROM files;
