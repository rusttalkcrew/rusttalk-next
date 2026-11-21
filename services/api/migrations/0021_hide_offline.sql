-- Leave this member out of other people's member lists while they appear offline.
ALTER TABLE members ADD COLUMN hide_offline INTEGER NOT NULL DEFAULT 0
    CHECK (hide_offline IN (0, 1));
