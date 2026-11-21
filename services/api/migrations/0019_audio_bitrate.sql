-- A voice channel's Opus bitrate in kbps, chosen by channel managers. Not user-written, so plain.
ALTER TABLE channels ADD COLUMN audio_kbps INTEGER NOT NULL DEFAULT 64 CHECK (audio_kbps BETWEEN 8 AND 128);
