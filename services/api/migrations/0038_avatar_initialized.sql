ALTER TABLE members ADD COLUMN avatar_initialized INTEGER NOT NULL DEFAULT 0;
UPDATE members SET avatar_initialized = 1 WHERE avatar_ct IS NOT NULL;
