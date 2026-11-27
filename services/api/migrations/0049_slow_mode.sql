-- Slow mode: the seconds each member waits between messages in a channel, 0 for off.
-- A setting, not user-written, so plain.
ALTER TABLE channels ADD COLUMN slow_mode_secs INTEGER NOT NULL DEFAULT 0 CHECK (slow_mode_secs >= 0);

-- `bypass_slow_mode` (65536) for the built-in staff roles and custom roles that may delete
-- messages, the ones that moderate chat.
UPDATE roles SET permissions = permissions | 65536
WHERE id IN ('owner', 'admin', 'moderator') OR (kind = 'custom' AND (permissions & 32) <> 0);
