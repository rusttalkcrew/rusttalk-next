-- open() marks upgrades from before events. A preview database may already
-- contain later roles and assignments, which cannot establish old grants.
CREATE TEMP TABLE IF NOT EXISTS upgrading_to_calendar (marker INTEGER);

CREATE TABLE calendar_role_grants (
    person_id BLOB NOT NULL REFERENCES members(person_id) ON DELETE CASCADE,
    role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    old_rank INTEGER NOT NULL,
    old_permissions BIGINT NOT NULL,
    retained BIGINT NOT NULL,
    PRIMARY KEY (person_id, role_id)
);

-- Aggregate the permissions across every assigned role. 0034 is the only
-- change to these grants since the pre-calendar server stopped.
WITH RECURSIVE bits(bit) AS (
    VALUES (1) UNION ALL SELECT bit * 2 FROM bits WHERE bit < 16384
), held AS (
    SELECT person_id, MAX(rank) AS rank, SUM(DISTINCT bit) AS permissions
    FROM member_roles JOIN roles ON roles.id = role_id
    LEFT JOIN bits ON (permissions & bit) <> 0
    WHERE EXISTS (SELECT 1 FROM upgrading_to_calendar)
    GROUP BY person_id
)
INSERT INTO calendar_role_grants
SELECT person_id, roles.id, roles.rank, roles.permissions & ~28672,
       roles.permissions & 28672 & ~held.permissions
FROM held JOIN roles ON roles.rank < held.rank
WHERE (held.permissions & 8) <> 0
  AND (roles.permissions & ~28672 & ~held.permissions) = 0
  AND (roles.permissions & 28672 & ~held.permissions) <> 0;

-- The old preview recorded only these member-to-Moderator grants.
CREATE TABLE IF NOT EXISTS legacy_moderator_granters (person_id BLOB PRIMARY KEY);
INSERT INTO calendar_role_grants
SELECT person_id, 'moderator', 50, 3118, 28672 FROM legacy_moderator_granters;
DROP TABLE legacy_moderator_granters;
DROP TABLE upgrading_to_calendar;

-- Once removed, adding a permission back needs the editor's own authority.
CREATE TRIGGER narrow_calendar_role_grants AFTER UPDATE OF permissions ON roles
WHEN (OLD.permissions & ~NEW.permissions & 28672) <> 0
BEGIN
    UPDATE calendar_role_grants SET retained = retained & NEW.permissions WHERE role_id = NEW.id;
END;
