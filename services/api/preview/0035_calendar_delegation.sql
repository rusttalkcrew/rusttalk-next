-- 0034 updated individual roles. Remember the remaining members who could
-- delegate Moderator through a combination of roles when the server upgraded.
CREATE TABLE legacy_moderator_granters (
    person_id BLOB NOT NULL PRIMARY KEY REFERENCES members(person_id) ON DELETE CASCADE
);

WITH held AS (
    SELECT person_id, rank, permissions FROM member_roles JOIN roles ON roles.id = role_id
)
INSERT INTO legacy_moderator_granters
SELECT person_id FROM held GROUP BY person_id
HAVING MAX(rank) > 50
   AND MAX(permissions & 8) > 0     -- manage_roles
   AND MAX(permissions & 2) > 0     -- kick
   AND MAX(permissions & 4) > 0     -- ban
   AND MAX(permissions & 32) > 0    -- delete_messages
   AND MAX(permissions & 1024) > 0  -- mute_members
   AND MAX(permissions & 2048) > 0  -- move_members
   AND (MAX(permissions & 8192) = 0 OR MAX(permissions & 16384) = 0);
