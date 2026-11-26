//! Checks on people's names. Names are end-to-end encrypted, so the server can't
//! check them: every app checks the names it sets and the names it shows.

/// Whether `name` holds a link: anything with `scheme://` (RustTalk invites
/// included, even broken ones), `www.`, or a bare domain followed by a path
/// (`example.com/join`). Dots alone are fine, so "J. Smith" and "dev.ops" pass.
pub fn contains_link(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("://")
        || lower.contains("www.")
        || lower.split_whitespace().any(domain_with_path)
}

/// `host.tld/…`: dot-separated labels of letters, digits and dashes, the last one
/// at least two letters, then a slash.
fn domain_with_path(word: &str) -> bool {
    let Some((host, _)) = word.split_once('/') else {
        return false;
    };
    let host = host.trim_start_matches(|c: char| !c.is_alphanumeric());
    let labels: Vec<&str> = host.split('.').collect();
    let tld = labels.last().copied().unwrap_or_default();
    labels.len() > 1
        && labels.iter().all(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        })
        && tld.chars().count() >= 2
        && tld.chars().all(char::is_alphabetic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_found_and_ordinary_names_pass() {
        let cases = [
            // Links and invites.
            ("rusttalk://chat.example.org/i/abc#def", true),
            ("join rusttalk://chat.example.org/i/abc#def now", true),
            ("https://example.com", true),
            ("HTTP://EXAMPLE.COM", true),
            ("ftp://files", true),
            ("www.example.com", true),
            ("Visit WWW.example.com", true),
            ("example.com/join", true),
            ("chat.example.org/i/abc", true),
            ("see (example.com/x)", true),
            ("sub-domain.example.co/path", true),
            // Ordinary names.
            ("Ada", false),
            ("J. Smith", false),
            ("dev.ops", false),
            ("Dr.Who", false),
            ("St. John-Smith", false),
            ("AC/DC", false),
            ("v1.2/3", false),
            ("1.5/10 gamer", false),
            ("a.b/c", false),
            ("Mr. T / Bob", false),
            ("Zoë 🎮", false),
            ("田中太郎", false),
            ("ゲーマー.jp", false),
            ("", false),
        ];
        for (name, link) in cases {
            assert_eq!(contains_link(name), link, "{name}");
        }
    }

    #[test]
    fn a_working_invite_is_a_link() {
        let invite = crate::auth::InviteLink {
            server: crate::auth::KnownServer {
                address: "https://chat.example.test/".parse().unwrap(),
                server_id: [1; 32],
                tls_fingerprint: Some([2; 32]),
            },
            code: [3; 16],
            link_key: None,
        }
        .to_string();
        assert!(crate::auth::InviteLink::parse(&invite).is_ok());
        assert!(contains_link(&invite));
        assert!(contains_link(&format!("Ann {invite}")));
    }
}
