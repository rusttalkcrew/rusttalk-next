-- A page channel is a text channel whose newest message is shown as one Markdown page.
-- A flag instead of a new kind, so the channels table and its keys need no rebuild.
ALTER TABLE channels ADD COLUMN page BOOLEAN NOT NULL DEFAULT FALSE CHECK (page IN (0, 1));
