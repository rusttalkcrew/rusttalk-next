ALTER TABLE server_settings ADD COLUMN max_file_bytes BIGINT CHECK (max_file_bytes > 0);
ALTER TABLE server_settings ADD COLUMN member_storage_bytes BIGINT CHECK (member_storage_bytes > 0);
ALTER TABLE server_settings ADD COLUMN server_storage_bytes BIGINT CHECK (server_storage_bytes > 0);
ALTER TABLE server_settings ADD COLUMN retention_secs BIGINT CHECK (retention_secs > 0);
ALTER TABLE server_settings ADD COLUMN max_stickers BIGINT CHECK (max_stickers > 0);

CREATE INDEX files_created ON files (created_at);
