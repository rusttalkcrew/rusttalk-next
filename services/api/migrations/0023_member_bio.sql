-- A member's profile bio, encrypted like their display name.
ALTER TABLE members ADD COLUMN bio_ct BLOB;
