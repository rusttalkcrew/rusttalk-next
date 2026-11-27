//! The official public servers, offered on the join page so new people can start in a click.
//! Nothing here connects anywhere: a server is contacted only when the person picks Join.

use rusttalk_core::{
    auth::{KnownServer, decode, encode},
    index::https_url,
};
use url::Url;

use crate::net::Listed;

/// The official server directory, used when the person sets no other.
/// It has a public certificate, so no pin.
pub const DIRECTORY: &str = "https://index.rusttalk.com/";

/// What the official directory accepts, linked from the listing page and the report dialog.
pub const DIRECTORY_RULES: &str =
    "https://github.com/rusttalkcrew/rusttalk-next/blob/main/docs/directory-rules.md";

/// Name, address and server ID (base64url). Each ID is that server's `rusttalk-api server-id`;
/// Join checks it like an invite would. Their certificates are public, so no pin.
const SERVERS: [(&str, &str, &str); 3] = [
    (
        "RustTalk Official Server (ES)",
        "https://es.public.rusttalk.com",
        "5KR7yOWg1h4P5xcgI5KP6RJ1JegGOl5Im16BaFG85ec",
    ),
    (
        "RustTalk Official Server (DE)",
        "https://de.public.rusttalk.com",
        "EoH97b_w-8249QRF-iQF-PwzHlEAuD0odRkV8nWGWUc",
    ),
    (
        "RustTalk Official Server (JP)",
        "https://jp.public.rusttalk.com",
        "jno3HrMja7Byv_Mq_YuF6s1hKu-OQXZSL8tdHKTMCWY",
    ),
];

/// The official servers not joined at `joined` and not dismissed (by server ID).
pub fn shown(joined: &[&Url], dismissed: &[String]) -> Vec<Listed> {
    SERVERS
        .iter()
        .filter(|(_, _, id)| !dismissed.iter().any(|gone| gone == id))
        .map(|&(name, address, id)| Listed {
            server: KnownServer {
                address: https_url(address).expect("official address"),
                server_id: decode(id).expect("official server ID"),
                tls_fingerprint: None,
            },
            name: name.into(),
            description: String::new(),
            members: 0,
            open: true,
        })
        .filter(|listed| !joined.contains(&&listed.server.address))
        .collect()
}

/// The server ID as stored in the dismissed list.
pub fn id(listed: &Listed) -> String {
    encode(&listed.server.server_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parses_and_filters() {
        let all = shown(&[], &[]);
        assert_eq!(all.len(), SERVERS.len());
        assert!(https_url(DIRECTORY).is_some());
        for (listed, (_, _, raw)) in all.iter().zip(SERVERS) {
            assert_eq!(listed.server.address.scheme(), "https");
            assert_eq!(id(listed), raw);
        }

        let joined = all[0].server.address.clone();
        let dismissed = vec![id(&all[2])];
        let left = shown(&[&joined], &dismissed);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, all[1].name);
    }
}
