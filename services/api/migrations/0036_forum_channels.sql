-- A forum channel is a text channel whose messages the app groups into posts and replies.
-- A flag like `page`, so the channels table and its keys need no rebuild.
ALTER TABLE channels ADD COLUMN forum BOOLEAN NOT NULL DEFAULT FALSE CHECK (forum IN (0, 1));
