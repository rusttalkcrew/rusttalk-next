//! What the UI shows for each server. Sign-in and sync fill `Home::servers` in
//! app.rs with these after decrypting the server's metadata; the UI only reads them.

pub struct Server {
    pub name: String,
    pub categories: Vec<Category>,
    /// Display names, already decrypted with the space key.
    pub members: Vec<String>,
}

/// Channels without a category go in a category with an empty name.
pub struct Category {
    pub name: String,
    pub channels: Vec<Channel>,
}

pub struct Channel {
    pub name: String,
    pub voice: bool,
}
