-- The stored files each version of a page channel keeps, as its editor listed them when
-- saving. Files the newest version keeps never expire; the others expire like any file.
-- Rows of deleted or pruned versions are removed when their page changes next.
CREATE TABLE page_files (
    message_id TEXT NOT NULL,
    file_id TEXT NOT NULL REFERENCES files (id) ON DELETE CASCADE,
    channel_id TEXT NOT NULL,
    PRIMARY KEY (message_id, file_id)
);
CREATE INDEX page_files_channel ON page_files (channel_id);
CREATE INDEX page_files_file ON page_files (file_id);
