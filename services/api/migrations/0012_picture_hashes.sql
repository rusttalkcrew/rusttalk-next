-- A short hash of each avatar and icon ciphertext lets clients fetch only pictures they lack.
-- The API fills these for existing pictures at startup.
ALTER TABLE members ADD COLUMN avatar_hash TEXT;
ALTER TABLE server_settings ADD COLUMN icon_hash TEXT;
