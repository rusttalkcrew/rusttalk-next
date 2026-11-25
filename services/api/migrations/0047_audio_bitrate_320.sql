-- Voice channels may use up to 320 kbps. A CHECK can't be altered in place, so the column is
-- replaced instead of rebuilding the table, which other tables reference and cascade from.
ALTER TABLE channels ADD COLUMN audio_kbps_wide INTEGER NOT NULL DEFAULT 64 CHECK (audio_kbps_wide BETWEEN 8 AND 320);
UPDATE channels SET audio_kbps_wide = audio_kbps;
ALTER TABLE channels DROP COLUMN audio_kbps;
ALTER TABLE channels RENAME COLUMN audio_kbps_wide TO audio_kbps;
