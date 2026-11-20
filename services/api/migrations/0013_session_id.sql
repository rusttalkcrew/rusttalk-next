ALTER TABLE sessions ADD COLUMN session_id BLOB;
UPDATE sessions SET session_id = token_hash;
