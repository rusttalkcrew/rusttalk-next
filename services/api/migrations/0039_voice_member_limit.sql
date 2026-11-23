-- The most people a voice channel lets in; 0 means no limit. A setting, not user-written, so plain.
ALTER TABLE channels ADD COLUMN member_limit INTEGER NOT NULL DEFAULT 0 CHECK (member_limit BETWEEN 0 AND 99);
