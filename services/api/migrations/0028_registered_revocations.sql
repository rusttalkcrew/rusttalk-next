ALTER TABLE revocations ADD COLUMN was_registered BOOLEAN NOT NULL DEFAULT FALSE;
CREATE TRIGGER remember_revoked_registration BEFORE DELETE ON member_devices
BEGIN
    UPDATE revocations SET was_registered = TRUE
    WHERE person_id = OLD.person_id AND device_id = OLD.device_id;
END;
