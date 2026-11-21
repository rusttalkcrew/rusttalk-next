-- Who made each invite, so people can see and stop their own. Older invites stay unattributed.
ALTER TABLE invites ADD COLUMN created_by BLOB;
