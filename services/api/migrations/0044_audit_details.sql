-- What an audit entry changed, as JSON: only opaque IDs, permission flags and numbers.
ALTER TABLE audit_log ADD COLUMN details TEXT;
