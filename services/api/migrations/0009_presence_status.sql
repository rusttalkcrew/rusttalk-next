-- Only the current choice is kept, never a history of changes.
ALTER TABLE members ADD COLUMN presence_status TEXT NOT NULL DEFAULT 'online'
    CHECK (presence_status IN ('online', 'away', 'invisible'));
