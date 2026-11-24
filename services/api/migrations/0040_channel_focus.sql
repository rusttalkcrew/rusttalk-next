-- Focus mode: until when (unix seconds) a channel asks visitors not to interrupt.
-- 0039 is left for an open pull request.
ALTER TABLE channels ADD COLUMN focus_until BIGINT;

-- `start_focus` (32768): every role may start focus mode until someone takes it away.
UPDATE roles SET permissions = permissions | 32768;
