-- Who answered an event, sealed so the server can't read the answer, and when the
-- author or a manager last reminded those who haven't answered.
ALTER TABLE calendar_events ADD COLUMN reminded_at BIGINT;

CREATE TABLE calendar_answers (
    event_id TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
    person BLOB NOT NULL,
    device BLOB NOT NULL,
    certificate BLOB NOT NULL,
    epoch BIGINT NOT NULL CHECK (epoch > 0),
    envelope BLOB NOT NULL,
    PRIMARY KEY (event_id, person)
);
