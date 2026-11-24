-- How many messages each channel keeps, so a post can tell when to trim without scanning
-- the 10,000 kept rows.
ALTER TABLE channels ADD COLUMN message_count INTEGER NOT NULL DEFAULT 0;
UPDATE channels SET message_count = (SELECT COUNT(*) FROM messages WHERE channel_id = channels.id);
