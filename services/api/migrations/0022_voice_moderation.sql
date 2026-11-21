-- Voice moderation: `mute_members` (1024) and `move_members` (2048) for the built-in staff
-- roles, and a server-wide mute kept on the member until lifted. Leaving or being removed
-- from the server deletes the member row, and the mute with it.
UPDATE roles SET permissions = permissions | 3072 WHERE id IN ('owner', 'admin', 'moderator');
ALTER TABLE members ADD COLUMN voice_muted INTEGER NOT NULL DEFAULT 0 CHECK (voice_muted IN (0, 1));
