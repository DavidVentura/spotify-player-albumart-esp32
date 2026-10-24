//! Server-to-panel stream: a sequence of `[kind: u8][length: u32 LE][payload]`
//! messages over one TCP connection.
//!
//! The server does all colour work; the panel receives the exact levels to
//! drive and only scales them by brightness.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// Edge length of the panel in pixels.
pub const SIZE: usize = 64;
/// Brightness levels per channel the panel shows, as a bit depth.
pub const PANEL_BITS: u32 = 6;

// Numbered past the RGB888 protocol's kinds so a panel running older firmware
// rejects this stream instead of misreading it
const KIND_FRAME: u8 = 3;
const KIND_BRIGHTNESS: u8 = 4;
const HEADER_LEN: usize = 5;
const FRAME_LEN: usize = SIZE * SIZE * 3;
const BRIGHTNESS_LEN: usize = 1;

/// One channel's drive level, `0..=Level::MAX`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(u8);

impl Level {
    pub const MAX: Level = Level((1 << PANEL_BITS) - 1);

    pub fn new(level: u8) -> Option<Self> {
        (level <= Self::MAX.0).then_some(Level(level))
    }

    pub fn value(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelColor {
    pub r: Level,
    pub g: Level,
    pub b: Level,
}

/// What every LED of the panel shows, indexed as `pixels[y][x]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelFrame {
    pub pixels: Box<[[PanelColor; SIZE]; SIZE]>,
}

impl PanelFrame {
    pub fn black() -> Self {
        // Built row by row on the heap; `Box::new` of the whole array would
        // first place 12 KiB on the stack, which is most of a microcontroller's
        let rows = alloc::vec![[PanelColor::default(); SIZE]; SIZE];
        let pixels = rows
            .into_boxed_slice()
            .try_into()
            .expect("SIZE rows were allocated");
        PanelFrame { pixels }
    }

    /// `levels` holds `SIZE * SIZE` row-major RGB triplets.
    fn decode(levels: &[u8]) -> Result<Self, ProtocolError> {
        let level = |byte: u8| Level::new(byte).ok_or(ProtocolError::LevelOutOfRange(byte));
        let rows = levels
            .chunks_exact(SIZE * 3)
            .map(|row| {
                let mut colors = [PanelColor::default(); SIZE];
                for (color, rgb) in colors.iter_mut().zip(row.chunks_exact(3)) {
                    *color = PanelColor {
                        r: level(rgb[0])?,
                        g: level(rgb[1])?,
                        b: level(rgb[2])?,
                    };
                }
                Ok(colors)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let pixels = rows
            .into_boxed_slice()
            .try_into()
            .expect("FRAME_LEN bytes hold exactly SIZE rows");
        Ok(PanelFrame { pixels })
    }
}

/// Panel brightness as a percentage of full output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brightness(u8);

impl Brightness {
    pub const OFF: Brightness = Brightness(0);

    pub const fn new(percent: u8) -> Option<Self> {
        if percent > 100 {
            return None;
        }
        Some(Brightness(percent))
    }

    pub fn percent(self) -> u8 {
        self.0
    }

    /// Fraction of full output in the 0..=255 scale the framebuffer uses.
    pub fn duty(self) -> u8 {
        ((u16::from(self.0) * 255 + 50) / 100) as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Frame(PanelFrame),
    Brightness(Brightness),
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload) = match self {
            Message::Frame(frame) => {
                let levels = frame
                    .pixels
                    .iter()
                    .flatten()
                    .flat_map(|color| [color.r.0, color.g.0, color.b.0])
                    .collect();
                (KIND_FRAME, levels)
            }
            Message::Brightness(brightness) => (KIND_BRIGHTNESS, Vec::from([brightness.0])),
        };
        let len = u32::try_from(payload.len()).expect("payloads are a few KiB");
        let mut encoded = Vec::with_capacity(HEADER_LEN + payload.len());
        encoded.push(kind);
        encoded.extend_from_slice(&len.to_le_bytes());
        encoded.extend_from_slice(&payload);
        encoded
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    UnknownKind(u8),
    WrongLength { kind: u8, len: usize },
    LevelOutOfRange(u8),
    InvalidBrightness(u8),
}

/// Reassembles messages from a byte stream.
#[derive(Default)]
pub struct MessageReader {
    pending: Vec<u8>,
}

impl MessageReader {
    /// Returns the messages completed by `data`. An error means the stream is
    /// out of sync and the connection should be dropped.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Message>, ProtocolError> {
        self.pending.extend_from_slice(data);
        let mut messages = Vec::new();
        let mut consumed = 0;
        while let Some((message, len)) = decode(&self.pending[consumed..])? {
            messages.push(message);
            consumed += len;
        }
        self.pending.drain(..consumed);
        Ok(messages)
    }
}

/// Decodes the first message in `data`, returning it with its encoded length,
/// or `None` if `data` does not hold a whole message yet.
fn decode(data: &[u8]) -> Result<Option<(Message, usize)>, ProtocolError> {
    let Some(header) = data.get(..HEADER_LEN) else {
        return Ok(None);
    };
    let kind = header[0];
    let len = u32::from_le_bytes(header[1..].try_into().expect("4 length bytes")) as usize;
    let expected = match kind {
        KIND_FRAME => FRAME_LEN,
        KIND_BRIGHTNESS => BRIGHTNESS_LEN,
        _ => return Err(ProtocolError::UnknownKind(kind)),
    };
    // Checked before waiting for the payload, so a corrupt length cannot make
    // the reader buffer an unbounded amount
    if len != expected {
        return Err(ProtocolError::WrongLength { kind, len });
    }
    let Some(payload) = data.get(HEADER_LEN..HEADER_LEN + len) else {
        return Ok(None);
    };
    let message = match kind {
        KIND_FRAME => Message::Frame(PanelFrame::decode(payload)?),
        _ => Message::Brightness(
            Brightness::new(payload[0]).ok_or(ProtocolError::InvalidBrightness(payload[0]))?,
        ),
    };
    Ok(Some((message, HEADER_LEN + len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn gradient() -> PanelFrame {
        let mut frame = PanelFrame::black();
        for (y, row) in frame.pixels.iter_mut().enumerate() {
            for (x, color) in row.iter_mut().enumerate() {
                *color = PanelColor {
                    r: Level::new(x as u8).unwrap(),
                    g: Level::new(y as u8).unwrap(),
                    b: Level::new(((x + y) / 2) as u8).unwrap(),
                };
            }
        }
        frame
    }

    fn brightness() -> Brightness {
        Brightness::new(35).unwrap()
    }

    #[test]
    fn round_trips_messages_split_at_every_byte() {
        let messages = vec![Message::Brightness(brightness()), Message::Frame(gradient())];
        let stream: Vec<u8> = messages.iter().flat_map(Message::encode).collect();
        for cut in 0..stream.len() {
            let mut reader = MessageReader::default();
            let mut decoded = reader.feed(&stream[..cut]).unwrap();
            decoded.extend(reader.feed(&stream[cut..]).unwrap());
            assert_eq!(decoded, messages, "split at {cut}");
        }
    }

    #[test]
    fn brightness_encoding_is_stable() {
        assert_eq!(
            Message::Brightness(brightness()).encode(),
            vec![4, 1, 0, 0, 0, 35]
        );
    }

    #[test]
    fn frame_payload_is_row_major_levels() {
        let encoded = Message::Frame(gradient()).encode();
        assert_eq!(&encoded[..HEADER_LEN], &[3, 0x00, 0x30, 0, 0]);
        // pixel (x=2, y=1)
        let offset = HEADER_LEN + (SIZE + 2) * 3;
        assert_eq!(&encoded[offset..offset + 3], &[2, 1, 1]);
    }

    #[test]
    fn levels_stop_at_the_panel_depth() {
        assert_eq!(Level::MAX.value(), 63);
        assert!(Level::new(64).is_none());
    }

    #[test]
    fn rejects_levels_beyond_the_panel_depth() {
        let mut encoded = Message::Frame(PanelFrame::black()).encode();
        encoded[HEADER_LEN + 7] = 64;
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.feed(&encoded),
            Err(ProtocolError::LevelOutOfRange(64))
        );
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.feed(&[9, 0, 0, 0, 0]),
            Err(ProtocolError::UnknownKind(9))
        );
    }

    #[test]
    fn rejects_the_rgb888_protocol() {
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.feed(&[1, 0x00, 0x30, 0, 0]),
            Err(ProtocolError::UnknownKind(1))
        );
    }

    #[test]
    fn rejects_wrong_length_before_buffering_payload() {
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.feed(&[3, 0xff, 0xff, 0xff, 0x7f]),
            Err(ProtocolError::WrongLength {
                kind: 3,
                len: 0x7fff_ffff
            })
        );
    }

    #[test]
    fn rejects_out_of_range_brightness() {
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.feed(&[4, 1, 0, 0, 0, 101]),
            Err(ProtocolError::InvalidBrightness(101))
        );
    }

    #[test]
    fn brightness_duty_spans_the_framebuffer_scale() {
        assert_eq!(Brightness::OFF.duty(), 0);
        assert_eq!(Brightness::new(100).unwrap().duty(), 255);
        assert_eq!(brightness().duty(), 89);
    }
}
