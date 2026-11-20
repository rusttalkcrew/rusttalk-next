ALTER TABLE server_settings ADD COLUMN open_joining BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE server_settings ADD COLUMN member_cap BIGINT NOT NULL DEFAULT 100 CHECK (member_cap > 0);

CREATE TABLE open_joins (
    ip TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);

ALTER TABLE bans ADD COLUMN banned_at BIGINT NOT NULL DEFAULT 0;
UPDATE bans
SET banned_at = COALESCE((
    SELECT at FROM audit_log
    WHERE action = 'member_banned' AND target_person_id = bans.person_id
    ORDER BY id DESC LIMIT 1
), 0);
