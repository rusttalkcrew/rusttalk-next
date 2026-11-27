-- The rate-limit preset someone with Manage Server picked. NULL until saved, when
-- "standard" (or the host's RUSTTALK_RATE_LIMITS) applies.
ALTER TABLE server_settings ADD COLUMN rate_limits TEXT
    CHECK (rate_limits IN ('strict', 'standard', 'relaxed'));
