//! G.711, the codec every phone network speaks: A-law (Europe) and µ-law (North
//! America), one byte per sample at 8 kHz (ITU-T G.711).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Law {
    /// RTP payload type 8.
    A,
    /// RTP payload type 0.
    Mu,
}

impl Law {
    pub fn payload_type(self) -> u8 {
        match self {
            Self::A => 8,
            Self::Mu => 0,
        }
    }

    pub fn from_payload_type(pt: u8) -> Option<Self> {
        match pt {
            8 => Some(Self::A),
            0 => Some(Self::Mu),
            _ => None,
        }
    }

    pub fn encode(self, sample: i16) -> u8 {
        match self {
            Self::A => alaw(sample),
            Self::Mu => ulaw(sample),
        }
    }

    pub fn decode(self, byte: u8) -> i16 {
        match self {
            Self::A => from_alaw(byte),
            Self::Mu => from_ulaw(byte),
        }
    }
}

/// The segment of a magnitude: the position of its highest bit above bit 7.
fn segment(magnitude: i32) -> i32 {
    let mut segment = 7;
    let mut mask = 0x4000;
    while segment > 0 && magnitude & mask == 0 {
        segment -= 1;
        mask >>= 1;
    }
    segment
}

fn alaw(sample: i16) -> u8 {
    let sample = i32::from(sample);
    // A-law's sign bit is set for positive samples.
    let (sign, magnitude) = if sample >= 0 {
        (0x80, sample)
    } else {
        (0, -sample - 1)
    };
    let code = if magnitude >= 0x100 {
        let segment = segment(magnitude);
        (segment << 4) | ((magnitude >> (segment + 3)) & 0x0f)
    } else {
        magnitude >> 4
    };
    ((sign | code) ^ 0x55) as u8
}

fn from_alaw(byte: u8) -> i16 {
    let byte = i32::from(byte ^ 0x55);
    let segment = (byte >> 4) & 0x07;
    let mut magnitude = ((byte & 0x0f) << 4) + 8;
    if segment != 0 {
        magnitude = (magnitude + 0x100) << (segment - 1);
    }
    (if byte & 0x80 != 0 {
        magnitude
    } else {
        -magnitude
    }) as i16
}

const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

fn ulaw(sample: i16) -> u8 {
    let sample = i32::from(sample);
    let (sign, magnitude) = if sample < 0 {
        (0x80, (-sample).min(CLIP))
    } else {
        (0, sample.min(CLIP))
    };
    let magnitude = magnitude + BIAS;
    let segment = segment(magnitude);
    let mantissa = (magnitude >> (segment + 3)) & 0x0f;
    !(sign | (segment << 4) | mantissa) as u8
}

fn from_ulaw(byte: u8) -> i16 {
    let byte = i32::from(!byte);
    let segment = (byte >> 4) & 0x07;
    let magnitude = ((((byte & 0x0f) << 3) + BIAS) << segment) - BIAS;
    (if byte & 0x80 != 0 {
        -magnitude
    } else {
        magnitude
    }) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // Silence is 0xd5 in A-law and 0xff in µ-law.
        assert_eq!(Law::A.encode(0), 0xd5);
        assert_eq!(Law::Mu.encode(0), 0xff);
        assert_eq!(Law::A.decode(0xd5), 8);
        assert_eq!(Law::Mu.decode(0xff), 0);
        assert_eq!(Law::Mu.encode(i16::MAX), 0x80);
        assert_eq!(Law::Mu.encode(i16::MIN), 0x00);
        assert_eq!(Law::A.encode(i16::MAX), 0xaa);
        assert_eq!(Law::A.encode(i16::MIN), 0x2a);
    }

    #[test]
    fn round_trips_stay_close() {
        for law in [Law::A, Law::Mu] {
            for sample in (i16::MIN..=i16::MAX).step_by(7) {
                let back = law.decode(law.encode(sample));
                let error = (i32::from(back) - i32::from(sample)).abs();
                // Each segment doubles the step; the error stays within half a step.
                let allowed = (i32::from(sample).abs() / 16).max(16) + 8;
                assert!(error <= allowed, "{law:?} {sample} -> {back}");
            }
            // Every byte decodes to a value that encodes back to the same byte, except
            // µ-law's negative zero (0x7f), which comes back as its positive zero.
            for byte in 0..=255u8 {
                let expected = if law == Law::Mu && byte == 0x7f {
                    0xff
                } else {
                    byte
                };
                assert_eq!(law.encode(law.decode(byte)), expected, "{law:?} {byte:#x}");
            }
        }
    }
}
