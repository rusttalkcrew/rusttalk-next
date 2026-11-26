-- Timeouts (#663): until when (unix seconds) a member may only read, and the moderator's
-- optional reason as space-key ciphertext. Not tied to the member row, so leaving and
-- rejoining doesn't end one early.
CREATE TABLE timeouts (
    person_id BLOB NOT NULL PRIMARY KEY,
    ends_at INTEGER NOT NULL,
    reason_ct BLOB
);

-- `timeout_members` (131072) for the built-in staff roles and custom roles that may kick,
-- the ones that already moderate members.
UPDATE roles SET permissions = permissions | 131072
WHERE id IN ('owner', 'admin', 'moderator') OR (kind = 'custom' AND (permissions & 2) <> 0);
