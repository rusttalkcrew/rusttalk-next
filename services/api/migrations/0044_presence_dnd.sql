-- Allow "dnd" (Do not disturb) as a stored presence choice. SQLite can't change a
-- CHECK constraint in place, so the column is rebuilt with its values.
ALTER TABLE members ADD COLUMN presence_choice TEXT NOT NULL DEFAULT 'online'
    CHECK (presence_choice IN ('online', 'away', 'dnd', 'invisible'));
UPDATE members SET presence_choice = presence_status;
ALTER TABLE members DROP COLUMN presence_status;
ALTER TABLE members RENAME COLUMN presence_choice TO presence_status;
