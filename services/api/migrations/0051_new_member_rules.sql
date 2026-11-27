-- New member rules: for this many seconds after joining, members with only the default
-- role can't upload files, start DMs or create invites. 0 is off. A setting, so plain.
ALTER TABLE server_settings ADD COLUMN new_member_secs INTEGER NOT NULL DEFAULT 0
    CHECK (new_member_secs >= 0 AND new_member_secs <= 2592000);

-- `skip_new_member_rules` (131072) for Owner and Admin, which hold every permission. Every
-- other role but the default skips the rules anyway, so Moderator and custom roles need no
-- upgrade; leaving them out keeps custom roles able to manage Moderator as before.
UPDATE roles SET permissions = permissions | 131072 WHERE id IN ('owner', 'admin');
