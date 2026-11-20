CREATE TABLE members (
    person_id BLOB NOT NULL PRIMARY KEY,
    joined_at BIGINT NOT NULL
);

CREATE TABLE roles (
    id TEXT NOT NULL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('owner', 'admin', 'moderator', 'member', 'custom')),
    rank INTEGER NOT NULL,
    permissions BIGINT NOT NULL,
    name_ct BLOB CHECK (length(name_ct) <= 512)
);

INSERT INTO roles (id, kind, rank, permissions) VALUES
    ('owner', 'owner', 1000, 63),
    ('admin', 'admin', 100, 63),
    ('moderator', 'moderator', 50, 38),
    ('member', 'member', 0, 0);

CREATE TABLE server_settings (
    id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    owner_person_id BLOB REFERENCES members(person_id),
    default_role_id TEXT NOT NULL REFERENCES roles(id)
);

INSERT INTO server_settings (id, default_role_id) VALUES (1, 'member');

CREATE TABLE member_roles (
    person_id BLOB NOT NULL REFERENCES members(person_id) ON DELETE CASCADE,
    role_id TEXT NOT NULL REFERENCES roles(id),
    PRIMARY KEY (person_id, role_id)
);

CREATE TABLE invites (
    id TEXT NOT NULL PRIMARY KEY,
    code_hash BLOB NOT NULL UNIQUE,
    is_owner BOOLEAN NOT NULL,
    remaining_uses INTEGER NOT NULL CHECK (remaining_uses > 0),
    expires_at BIGINT NOT NULL
);

CREATE TABLE challenges (
    nonce BLOB NOT NULL PRIMARY KEY,
    expires_at BIGINT NOT NULL
);

CREATE TABLE sessions (
    token_hash BLOB NOT NULL PRIMARY KEY,
    person_id BLOB NOT NULL REFERENCES members(person_id),
    device_id BLOB NOT NULL,
    ip TEXT NOT NULL,
    expires_at BIGINT NOT NULL,
    UNIQUE (person_id, device_id)
);

CREATE INDEX sessions_expiry ON sessions (expires_at);

CREATE TABLE revocations (
    person_id BLOB NOT NULL,
    device_id BLOB NOT NULL,
    record BLOB NOT NULL,
    PRIMARY KEY (person_id, device_id)
);

CREATE TABLE bans (
    person_id BLOB NOT NULL PRIMARY KEY
);

CREATE TABLE ban_ips (
    person_id BLOB NOT NULL REFERENCES bans(person_id) ON DELETE CASCADE,
    ip TEXT NOT NULL,
    PRIMARY KEY (person_id, ip)
);

CREATE INDEX banned_addresses ON ban_ips (ip);

CREATE TABLE audit_log (
    id BIGINT NOT NULL PRIMARY KEY,
    actor_person_id BLOB NOT NULL,
    action TEXT NOT NULL,
    target_person_id BLOB,
    target_id TEXT,
    at BIGINT NOT NULL
);
