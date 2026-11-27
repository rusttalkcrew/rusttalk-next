-- Raid tools (#665). Lockdown: while on, nobody new joins, by invite or open joining.
-- Its optional message is for people who aren't members and hold no space key, so it is
-- sealed with a key derived from the server key, like the public listing.
ALTER TABLE server_settings ADD COLUMN locked_down BOOLEAN NOT NULL DEFAULT FALSE;
-- Whether the join-spike detector turned it on, rather than a person.
ALTER TABLE server_settings ADD COLUMN locked_automatically BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE server_settings ADD COLUMN lockdown_message_ct BLOB;
-- Automatic lockdown: this many joins within this many seconds turns it on; 0 joins is off.
ALTER TABLE server_settings ADD COLUMN auto_lockdown_joins INTEGER NOT NULL DEFAULT 10
    CHECK (auto_lockdown_joins >= 0);
ALTER TABLE server_settings ADD COLUMN auto_lockdown_secs INTEGER NOT NULL DEFAULT 120
    CHECK (auto_lockdown_secs > 0);

-- `lock_down` (524288) for the built-in staff roles and custom roles that may ban.
UPDATE roles SET permissions = permissions | 524288
WHERE id IN ('owner', 'admin', 'moderator') OR (kind = 'custom' AND (permissions & 4) <> 0);
-- `remove_recent_messages` (1048576) for the built-in staff roles and custom roles that may
-- both ban and delete messages.
UPDATE roles SET permissions = permissions | 1048576
WHERE id IN ('owner', 'admin', 'moderator') OR (kind = 'custom' AND (permissions & 36) = 36);
