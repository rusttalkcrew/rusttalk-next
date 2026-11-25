-- Who placed each ban and their rank at that moment, so only the owner, the placer or
-- someone who outranks them can lift it. Bans placed before this are filled in from the
-- audit log; rows whose placer is unknown or no longer a member keep a NULL rank, and
-- anyone with the ban permission can lift those, as before.
ALTER TABLE bans ADD COLUMN banned_by BLOB;
ALTER TABLE bans ADD COLUMN banned_by_rank BIGINT;
UPDATE bans
SET banned_by = (
    SELECT actor_person_id FROM audit_log
    WHERE action = 'member_banned' AND target_person_id = bans.person_id
    ORDER BY id DESC LIMIT 1
);
UPDATE bans
SET banned_by_rank = (
    SELECT MAX(rank) FROM roles WHERE id IN (
        SELECT role_id FROM member_roles WHERE person_id = bans.banned_by
        UNION ALL SELECT 'owner' FROM server_settings WHERE owner_person_id = bans.banned_by
    )
)
WHERE banned_by IS NOT NULL;
