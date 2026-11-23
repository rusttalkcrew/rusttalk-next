//! RTP packets (RFC 3550) for the call's sound, and keypad tones as telephone events
//! (RFC 4733). The sound itself is G.711 at 8 kHz in 20 ms packets.

/// Samples in a 20 ms packet at 8 kHz.
pub const PACKET: usize = 160;
const HEADER: usize = 12;

pub struct Packet<'a> {
    pub payload_type: u8,
    pub sequence: u16,
    pub payload: &'a [u8],
}

/// Read an RTP packet; `None` for anything else.
pub fn parse(data: &[u8]) -> Option<Packet<'_>> {
    if data.len() < HEADER || data[0] >> 6 != 2 {
        return None;
    }
    let padding = data[0] & 0x20 != 0;
    let extension = data[0] & 0x10 != 0;
    let csrcs = usize::from(data[0] & 0x0f);
    let mut start = HEADER + 4 * csrcs;
    if extension {
        let words = data.get(start + 2..start + 4)?;
        start += 4 + 4 * usize::from(u16::from_be_bytes([words[0], words[1]]));
    }
    let mut end = data.len();
    if padding {
        end = end.checked_sub(usize::from(*data.last()?))?;
    }
    Some(Packet {
        payload_type: data[1] & 0x7f,
        sequence: u16::from_be_bytes([data[2], data[3]]),
        payload: data.get(start..end)?,
    })
}

/// What we send: the stream's counters.
pub struct Sender {
    pub ssrc: u32,
    pub sequence: u16,
    /// Of the next sound packet, in 8 kHz samples.
    pub timestamp: u32,
}

impl Sender {
    pub fn new(ssrc: u32, sequence: u16, timestamp: u32) -> Self {
        Self {
            ssrc,
            sequence,
            timestamp,
        }
    }

    fn packet(
        &mut self,
        payload_type: u8,
        marker: bool,
        timestamp: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut packet = Vec::with_capacity(HEADER + payload.len());
        packet.push(0x80);
        packet.push(payload_type | if marker { 0x80 } else { 0 });
        packet.extend(self.sequence.to_be_bytes());
        packet.extend(timestamp.to_be_bytes());
        packet.extend(self.ssrc.to_be_bytes());
        packet.extend(payload);
        self.sequence = self.sequence.wrapping_add(1);
        packet
    }

    /// 20 ms of sound.
    pub fn sound(&mut self, payload_type: u8, payload: &[u8]) -> Vec<u8> {
        let timestamp = self.timestamp;
        self.timestamp = self.timestamp.wrapping_add(PACKET as u32);
        self.packet(payload_type, false, timestamp, payload)
    }

    /// Stand in for a sound packet while a tone plays: time goes on, nothing is sent.
    pub fn skip(&mut self) {
        self.timestamp = self.timestamp.wrapping_add(PACKET as u32);
    }

    /// One packet of a tone that started at `start`.
    pub fn tone(&mut self, payload_type: u8, start: u32, tone: &Tone) -> Vec<u8> {
        let payload = tone.payload();
        self.packet(payload_type, tone.sent == 0, start, &payload)
    }
}

/// Packets a tone lasts: 100 ms, which every phone menu takes.
const TONE_PACKETS: u16 = 5;
/// The end packet is sent three times, as RFC 4733 recommends against loss.
const END_PACKETS: u16 = 3;

/// A keypad tone being sent, one packet per 20 ms.
pub struct Tone {
    event: u8,
    sent: u16,
}

impl Tone {
    pub fn new(key: char) -> Option<Self> {
        let event = match key {
            '0'..='9' => key as u8 - b'0',
            '*' => 10,
            '#' => 11,
            _ => return None,
        };
        Some(Self { event, sent: 0 })
    }

    fn ended(&self) -> bool {
        self.sent >= TONE_PACKETS
    }

    pub fn done(&self) -> bool {
        self.sent >= TONE_PACKETS + END_PACKETS
    }

    fn payload(&self) -> [u8; 4] {
        let duration = (self.sent.min(TONE_PACKETS - 1) + 1) * PACKET as u16;
        // Volume 10 (-10 dBm0); the top bit marks the end.
        let flags = if self.ended() { 0x80 | 10 } else { 10 };
        let [high, low] = duration.to_be_bytes();
        [self.event, flags, high, low]
    }

    pub fn advance(&mut self) {
        self.sent += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(packet: &[u8]) -> u32 {
        u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]])
    }

    #[test]
    fn packets_read_back() {
        let mut sender = Sender::new(0x1234_5678, u16::MAX, 160);
        let first = sender.sound(8, &[1, 2, 3]);
        let second = sender.sound(8, &[4]);
        let packet = parse(&first).unwrap();
        assert_eq!(packet.payload_type, 8);
        assert_eq!(packet.sequence, u16::MAX);
        assert_eq!(timestamp(&first), 160);
        assert_eq!(packet.payload, &[1, 2, 3]);
        let packet = parse(&second).unwrap();
        assert_eq!(packet.sequence, 0);
        assert_eq!(timestamp(&second), 320);
        assert!(parse(&first[..11]).is_none());
        assert!(parse(&[0u8; 20]).is_none());
    }

    #[test]
    fn padding_csrcs_and_extensions_are_skipped() {
        let mut data = vec![0xb1, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        data.extend([9, 9, 9, 9]); // one CSRC
        data.extend([0xbe, 0xde, 0, 1, 7, 7, 7, 7]); // a one-word extension
        data.extend([5, 6, 0, 0, 3]); // payload, then 3 bytes of padding
        let packet = parse(&data).unwrap();
        assert_eq!(packet.payload, &[5, 6]);
        data.truncate(data.len() - 1);
        data.push(40);
        assert!(parse(&data).is_none());
    }

    #[test]
    fn a_tone_grows_then_ends_three_times() {
        let mut sender = Sender::new(1, 0, 0);
        let mut tone = Tone::new('#').unwrap();
        let mut packets = Vec::new();
        while !tone.done() {
            packets.push(sender.tone(101, 480, &tone));
            tone.advance();
        }
        assert_eq!(packets.len(), 8);
        let read: Vec<_> = packets.iter().map(|p| parse(p).unwrap()).collect();
        assert!(packets[0][1] & 0x80 != 0, "the first packet is marked");
        assert!(packets[1][1] & 0x80 == 0);
        assert!(packets.iter().all(|p| timestamp(p) == 480));
        assert!(read.iter().all(|p| p.payload_type == 101));
        assert_eq!(read[0].payload, &[11, 10, 0, 160]);
        assert_eq!(read[4].payload, &[11, 10, 3, 32]);
        for end in &read[5..] {
            assert_eq!(end.payload, &[11, 0x80 | 10, 3, 32]);
        }
        assert!(Tone::new('x').is_none());
        assert_eq!(Tone::new('7').unwrap().event, 7);
        assert_eq!(Tone::new('*').unwrap().event, 10);
    }
}
