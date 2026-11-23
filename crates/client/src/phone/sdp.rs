//! The session description (RFC 4566) of a call: our offer of G.722, G.711 and keypad tones,
//! and what the other side answered.

use std::net::{IpAddr, SocketAddr};

use super::codec::{Codec, G722};

/// The payload type we offer for keypad tones.
pub const TONES: u8 = 101;

pub fn offer(address: IpAddr, port: u16, session: u64) -> String {
    let family = if address.is_ipv4() { "IP4" } else { "IP6" };
    [
        "v=0".to_owned(),
        format!("o=rusttalk {session} {session} IN {family} {address}"),
        "s=RustTalk".to_owned(),
        format!("c=IN {family} {address}"),
        "t=0 0".to_owned(),
        // Wideband first, for clearer voices; then A-law, as the people asking for
        // this are in Europe; then µ-law, which every phone service has.
        format!("m=audio {port} RTP/AVP {G722} 8 0 {TONES}"),
        format!("a=rtpmap:{G722} G722/8000"),
        "a=rtpmap:8 PCMA/8000".to_owned(),
        "a=rtpmap:0 PCMU/8000".to_owned(),
        format!("a=rtpmap:{TONES} telephone-event/8000"),
        format!("a=fmtp:{TONES} 0-15"),
        "a=ptime:20".to_owned(),
        "a=sendrecv".to_owned(),
        String::new(),
    ]
    .join("\r\n")
}

#[derive(Debug, PartialEq)]
pub struct Answer {
    /// Where to send our sound.
    pub to: SocketAddr,
    pub codec: Codec,
    /// The payload type for keypad tones, if the other side takes them.
    pub tones: Option<u8>,
}

/// The other side's answer; `None` when it has none of our codecs.
pub fn answer(sdp: &str) -> Option<Answer> {
    let mut session_address = None;
    let mut media_address = None;
    let mut media: Option<(u16, Vec<u8>)> = None;
    let mut tones = None;
    let mut in_audio = false;
    let mut seen_media = false;
    let mut direction = "sendrecv";
    for line in sdp.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("m=") {
            seen_media = true;
            in_audio = media.is_none() && rest.starts_with("audio ");
            if in_audio {
                let mut parts = rest.split_whitespace().skip(1);
                let port = parts.next()?.parse().ok()?;
                if parts.next()? != "RTP/AVP" {
                    return None;
                }
                media = Some((port, parts.filter_map(|pt| pt.parse().ok()).collect()));
            }
        } else if let Some(rest) = line.strip_prefix("c=") {
            if !in_audio && seen_media {
                continue;
            }
            let parts: Vec<_> = rest.split_whitespace().collect();
            if parts.len() != 3 || parts[0] != "IN" {
                return None;
            }
            let address: IpAddr = parts[2].parse().ok()?;
            if (parts[1] != "IP4" || !address.is_ipv4())
                && (parts[1] != "IP6" || !address.is_ipv6())
            {
                return None;
            }
            if address.is_unspecified()
                || address.is_multicast()
                || address == IpAddr::V4(std::net::Ipv4Addr::BROADCAST)
            {
                return None;
            }
            let address = Some(address);
            if in_audio {
                media_address = address;
            } else if media.is_none() {
                session_address = address;
            }
        } else if (!seen_media || in_audio)
            && matches!(
                line,
                "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
            )
        {
            direction = &line[2..];
        } else if in_audio && let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let (pt, codec) = rest.split_once(' ')?;
            let mono = codec.strip_suffix("/1").unwrap_or(codec);
            if (pt == "8" && !mono.eq_ignore_ascii_case("PCMA/8000"))
                || (pt == "0" && !mono.eq_ignore_ascii_case("PCMU/8000"))
                || (pt == "9" && !mono.eq_ignore_ascii_case("G722/8000"))
            {
                return None;
            }
            if codec.to_ascii_lowercase().eq("telephone-event/8000") {
                tones = pt.parse().ok();
            }
        }
    }
    let (port, payload_types) = media?;
    // Hold and one-way calls are not supported; never export sound against direction.
    if port == 0 || direction != "sendrecv" {
        return None;
    }
    // The answer lists the codecs in the order the other side prefers them.
    let codec = payload_types
        .iter()
        .find_map(|&pt| Codec::from_payload_type(pt))?;
    let tones = tones.filter(|pt| payload_types.contains(pt));
    Some(Answer {
        to: SocketAddr::new(media_address.or(session_address)?, port),
        codec,
        tones,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone::g711::Law;

    #[test]
    fn unsupported_or_unsafe_answers_are_rejected() {
        let valid = offer("192.0.2.7".parse().unwrap(), 40000, 99);
        for (from, to) in [
            ("RTP/AVP", "RTP/SAVP"),
            ("RTP/AVP", "UDP/TLS/RTP/SAVPF"),
            ("IN IP4 192.0.2.7", "IN IP6 192.0.2.7"),
            ("IN IP4 192.0.2.7", "XX IP4 192.0.2.7"),
            ("192.0.2.7", "0.0.0.0"),
            ("192.0.2.7", "224.0.0.1"),
            ("192.0.2.7", "255.255.255.255"),
            ("192.0.2.7", "192.0.2.7/2"),
            ("sendrecv", "sendonly"),
            ("sendrecv", "recvonly"),
            ("sendrecv", "inactive"),
            ("PCMA/8000", "opus/48000"),
        ] {
            assert!(answer(&valid.replace(from, to)).is_none(), "{to}");
        }
    }

    #[test]
    fn provider_can_specify_one_channel() {
        for (pt, codec, want) in [
            (0, "PCMU", Codec::G711(Law::Mu)),
            (8, "PCMA", Codec::G711(Law::A)),
            (9, "G722", Codec::G722),
        ] {
            for (params, valid) in [
                ("8000", true),
                ("8000/1", true),
                ("8000/2", false),
                ("16000/1", false),
            ] {
                let sdp = format!(
                    "c=IN IP4 192.0.2.9\nm=audio 6000 RTP/AVP {pt}\na=rtpmap:{pt} {codec}/{params}\n"
                );
                assert_eq!(answer(&sdp).map(|a| a.codec), valid.then_some(want));
            }
        }
    }

    #[test]
    fn our_offer_reads_as_an_answer() {
        let sdp = offer("192.0.2.7".parse().unwrap(), 40000, 99);
        assert!(sdp.ends_with("\r\n"));
        assert_eq!(
            answer(&sdp),
            Some(Answer {
                to: "192.0.2.7:40000".parse().unwrap(),
                codec: Codec::G722,
                tones: Some(TONES),
            })
        );
    }

    #[test]
    fn answers_pick_g711_and_the_media_address() {
        let sdp = "v=0\r\no=- 1 1 IN IP4 198.51.100.1\r\ns=-\r\nc=IN IP4 198.51.100.1\r\nt=0 0\r\n\
                   m=audio 30000 RTP/AVP 0 96\r\nc=IN IP4 203.0.113.5\r\n\
                   a=rtpmap:0 PCMU/8000\r\na=rtpmap:96 telephone-event/8000\r\n\
                   m=video 0 RTP/AVP 31\r\n";
        let got = answer(sdp).unwrap();
        assert_eq!(got.to, "203.0.113.5:30000".parse().unwrap());
        assert_eq!(got.codec, Codec::G711(Law::Mu));
        assert_eq!(got.tones, Some(96));
        // Without a media address the session's applies; without tones there are none.
        let sdp = "v=0\nc=IN IP4 198.51.100.1\nm=audio 30000 RTP/AVP 8\n";
        let got = answer(sdp).unwrap();
        assert_eq!(got.to, "198.51.100.1:30000".parse().unwrap());
        assert_eq!(got.tones, None);
        // Refused audio, or only codecs we lack.
        assert_eq!(answer("c=IN IP4 198.51.100.1\nm=audio 0 RTP/AVP 8\n"), None);
        assert_eq!(
            answer("c=IN IP4 198.51.100.1\nm=audio 5 RTP/AVP 18\n"),
            None
        );
        assert_eq!(answer(""), None);
        // The other side's order wins: here G.711 before G.722.
        let got = answer("c=IN IP4 198.51.100.1\nm=audio 5 RTP/AVP 8 9\n").unwrap();
        assert_eq!(got.codec, Codec::G711(Law::A));
    }
}
