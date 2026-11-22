-- A page channel's document: one signed ciphertext that each save replaces. The
-- channel's key scope cascades, so deleting the channel deletes its document.
CREATE TABLE page_documents (
    channel_id TEXT NOT NULL PRIMARY KEY,
    version BIGINT NOT NULL CHECK (version > 0),
    epoch BIGINT NOT NULL,
    sender_device BLOB NOT NULL,
    sender_person BLOB NOT NULL,
    at BIGINT NOT NULL,
    size INTEGER NOT NULL CHECK (size = length(envelope) AND size BETWEEN 105 AND 65536),
    envelope BLOB NOT NULL,
    FOREIGN KEY (channel_id, epoch) REFERENCES key_epochs(scope_id, epoch) ON DELETE CASCADE,
    FOREIGN KEY (sender_device, sender_person) REFERENCES message_senders(device_id, person_id)
);

-- The stored files a page's document keeps. page_files stays as it is: until a channel
-- has a document, what its last message-based page kept is not given up.
CREATE TABLE page_document_files (
    channel_id TEXT NOT NULL REFERENCES page_documents(channel_id) ON DELETE CASCADE,
    file_id TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    PRIMARY KEY (channel_id, file_id)
);
CREATE INDEX page_document_files_file ON page_document_files (file_id);
