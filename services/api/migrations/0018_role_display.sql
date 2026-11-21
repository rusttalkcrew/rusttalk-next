-- A role's colour and member-list grouping, sealed like its name.
ALTER TABLE roles ADD COLUMN display_ct BLOB CHECK (length(display_ct) <= 1100);
