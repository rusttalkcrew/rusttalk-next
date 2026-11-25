-- How many messages each channel keeps, as someone with Manage Server saved it; 0 keeps them
-- all. NULL until saved, when the default (or the host's RUSTTALK_MESSAGE_RETAIN) applies.
ALTER TABLE server_settings ADD COLUMN message_history BIGINT CHECK (message_history >= 0);
