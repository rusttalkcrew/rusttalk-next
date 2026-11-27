-- NSFW channels: whether a channel may hold mature content, so apps warn before showing it.
-- A setting, not user-written, so plain.
ALTER TABLE channels ADD COLUMN nsfw BOOLEAN NOT NULL DEFAULT FALSE;
