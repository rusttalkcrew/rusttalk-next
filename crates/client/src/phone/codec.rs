//! The call's sound: G.722 (wideband, up to 7 kHz) when the other side takes it,
//! else G.711 (narrowband, up to 3.4 kHz), which every phone service has.

use audio_codec::{
    Decoder, Encoder,
    g722::{G722Decoder, G722Encoder},
};

use super::g711::Law;

/// G.722's static payload type (RFC 3551). Its RTP clock runs at 8 kHz for historic
/// reasons, like G.711's, though it samples at 16 kHz.
pub const G722: u8 = 9;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Codec {
    G722,
    G711(Law),
}

impl Codec {
    pub fn from_payload_type(pt: u8) -> Option<Self> {
        if pt == G722 {
            Some(Self::G722)
        } else {
            Law::from_payload_type(pt).map(Self::G711)
        }
    }

    pub fn payload_type(self) -> u8 {
        match self {
            Self::G722 => G722,
            Self::G711(law) => law.payload_type(),
        }
    }

    /// The call's 48 kHz samples per codec sample.
    pub fn ratio(self) -> usize {
        match self {
            Self::G722 => 3,
            Self::G711(_) => 6,
        }
    }
}

/// Encodes and decodes one call's sound; G.722 keeps state between packets.
pub struct Coder {
    codec: Codec,
    encoder: G722Encoder,
    decoder: G722Decoder,
}

impl Coder {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            encoder: G722Encoder::new(),
            decoder: G722Decoder::new(),
        }
    }

    pub fn encode(&mut self, samples: &[i16]) -> Vec<u8> {
        match self.codec {
            Codec::G722 => self.encoder.encode(samples),
            Codec::G711(law) => samples.iter().map(|&s| law.encode(s)).collect(),
        }
    }

    /// `None` for a packet in a codec other than the agreed one.
    pub fn decode(&mut self, pt: u8, payload: &[u8]) -> Option<Vec<i16>> {
        match (self.codec, Codec::from_payload_type(pt)?) {
            (Codec::G722, Codec::G722) => Some(self.decoder.decode(payload)),
            // Some servers switch between the two laws; both decode the same way.
            (Codec::G711(_), Codec::G711(law)) => {
                Some(payload.iter().map(|&b| law.decode(b)).collect())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g722_packets_carry_20_ms_and_come_back() {
        let tone: Vec<i16> = (0..320)
            .map(|n| {
                (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 16_000.0).sin()) as i16
            })
            .collect();
        let mut coder = Coder::new(Codec::G722);
        let mut back = Vec::new();
        for _ in 0..5 {
            let packet = coder.encode(&tone);
            assert_eq!(packet.len(), 160, "64 kbit/s for 20 ms");
            back = coder.decode(G722, &packet).unwrap();
        }
        assert_eq!(back.len(), 320);
        let level = |s: &[i16]| s.iter().map(|&v| f64::from(v).abs()).sum::<f64>() / s.len() as f64;
        assert!((level(&back) / level(&tone) - 1.0).abs() < 0.2);
        // A G.711 packet is not G.722 sound.
        assert_eq!(coder.decode(8, &[0xd5; 160]), None);
        assert_eq!(Codec::from_payload_type(0), Some(Codec::G711(Law::Mu)));
    }
}
