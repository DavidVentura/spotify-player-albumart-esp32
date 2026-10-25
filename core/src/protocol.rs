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

/// Where decoded content goes. Pixels arrive one at a time as their bytes
/// do, so a frame can be written straight into its destination.
pub trait Target {
    fn pixel(&mut self, x: usize, y: usize, color: PanelColor);
    fn brightness(&mut self, brightness: Brightness);
}

/// How far one call to [`StreamDecoder::feed`] got.
#[derive(Debug, PartialEq, Eq)]
pub struct Progress {
    pub consumed: usize,
    /// The last pixel of a frame was just delivered. Decoding stops right
    /// there, so pixels of the next frame cannot reach the target before
    /// the caller has shown this one.
    pub frame_complete: bool,
}

/// Decodes the stream incrementally, without buffering whole messages.
#[derive(Default)]
pub struct StreamDecoder {
    state: DecodeState,
    header: HeaderBuffer,
}

#[derive(Default)]
enum DecodeState {
    #[default]
    Header,
    Frame {
        /// Payload bytes of this frame consumed so far.
        offset: usize,
        /// The current pixel's channels received so far.
        channels: [u8; 3],
    },
    Brightness,
}

impl StreamDecoder {
    /// Feeds `data`; call again with the unconsumed rest. An error means the
    /// stream is out of sync and the connection should be dropped.
    pub fn feed(
        &mut self,
        data: &[u8],
        target: &mut impl Target,
    ) -> Result<Progress, ProtocolError> {
        let mut consumed = 0;
        while consumed < data.len() {
            match &mut self.state {
                DecodeState::Header => {
                    consumed += self.header.fill(&data[consumed..]);
                    let Some((kind, len)) = self.header.take() else {
                        continue;
                    };
                    self.state = match (kind, len) {
                        (KIND_FRAME, FRAME_LEN) => DecodeState::Frame {
                            offset: 0,
                            channels: [0; 3],
                        },
                        (KIND_BRIGHTNESS, BRIGHTNESS_LEN) => DecodeState::Brightness,
                        (KIND_FRAME | KIND_BRIGHTNESS, len) => {
                            return Err(ProtocolError::WrongLength { kind, len });
                        }
                        (kind, _) => return Err(ProtocolError::UnknownKind(kind)),
                    };
                }
                DecodeState::Brightness => {
                    let percent = data[consumed];
                    consumed += 1;
                    let brightness = Brightness::new(percent)
                        .ok_or(ProtocolError::InvalidBrightness(percent))?;
                    target.brightness(brightness);
                    self.state = DecodeState::Header;
                }
                DecodeState::Frame { offset, channels } => {
                    let byte = data[consumed];
                    consumed += 1;
                    channels[*offset % 3] = byte;
                    *offset += 1;
                    if *offset % 3 != 0 {
                        continue;
                    }
                    let level =
                        |byte: u8| Level::new(byte).ok_or(ProtocolError::LevelOutOfRange(byte));
                    let pixel = *offset / 3 - 1;
                    let color = PanelColor {
                        r: level(channels[0])?,
                        g: level(channels[1])?,
                        b: level(channels[2])?,
                    };
                    target.pixel(pixel % SIZE, pixel / SIZE, color);
                    if *offset == FRAME_LEN {
                        self.state = DecodeState::Header;
                        return Ok(Progress {
                            consumed,
                            frame_complete: true,
                        });
                    }
                }
            }
        }
        Ok(Progress {
            consumed,
            frame_complete: false,
        })
    }
}

/// A message header that may arrive split across reads.
#[derive(Default)]
struct HeaderBuffer {
    bytes: [u8; HEADER_LEN],
    filled: usize,
}

impl HeaderBuffer {
    fn fill(&mut self, data: &[u8]) -> usize {
        let taken = data.len().min(HEADER_LEN - self.filled);
        self.bytes[self.filled..self.filled + taken].copy_from_slice(&data[..taken]);
        self.filled += taken;
        taken
    }

    fn take(&mut self) -> Option<(u8, usize)> {
        if self.filled < HEADER_LEN {
            return None;
        }
        self.filled = 0;
        let len = u32::from_le_bytes(self.bytes[1..].try_into().expect("4 length bytes"));
        Some((self.bytes[0], len as usize))
    }
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

    /// Rebuilds whole messages from what the decoder delivers.
    struct Collect {
        frame: PanelFrame,
        decoded: Vec<Message>,
    }

    impl Collect {
        fn new() -> Self {
            Collect {
                frame: PanelFrame::black(),
                decoded: Vec::new(),
            }
        }

        /// Feeds `data` the way a caller does, finishing a frame whenever
        /// the decoder reports one.
        fn feed(
            &mut self,
            decoder: &mut StreamDecoder,
            mut data: &[u8],
        ) -> Result<(), ProtocolError> {
            while !data.is_empty() {
                let progress = decoder.feed(data, self)?;
                if progress.frame_complete {
                    self.decoded.push(Message::Frame(self.frame.clone()));
                }
                data = &data[progress.consumed..];
            }
            Ok(())
        }
    }

    impl Target for Collect {
        fn pixel(&mut self, x: usize, y: usize, color: PanelColor) {
            self.frame.pixels[y][x] = color;
        }

        fn brightness(&mut self, brightness: Brightness) {
            self.decoded.push(Message::Brightness(brightness));
        }
    }

    fn decode(stream: &[u8]) -> Result<Vec<Message>, ProtocolError> {
        let mut decoder = StreamDecoder::default();
        let mut collect = Collect::new();
        collect.feed(&mut decoder, stream)?;
        Ok(collect.decoded)
    }

    #[test]
    fn round_trips_messages_split_at_every_byte() {
        let messages = vec![
            Message::Brightness(brightness()),
            Message::Frame(gradient()),
            Message::Frame(PanelFrame::black()),
        ];
        let stream: Vec<u8> = messages.iter().flat_map(Message::encode).collect();
        for cut in 0..stream.len() {
            let mut decoder = StreamDecoder::default();
            let mut collect = Collect::new();
            collect.feed(&mut decoder, &stream[..cut]).unwrap();
            collect.feed(&mut decoder, &stream[cut..]).unwrap();
            assert_eq!(collect.decoded, messages, "split at {cut}");
        }
    }

    #[test]
    fn stops_right_after_a_frame() {
        let mut stream = Message::Frame(gradient()).encode();
        let frame_len = stream.len();
        stream.extend(Message::Frame(PanelFrame::black()).encode());
        let mut decoder = StreamDecoder::default();
        let mut collect = Collect::new();
        assert_eq!(
            decoder.feed(&stream, &mut collect),
            Ok(Progress {
                consumed: frame_len,
                frame_complete: true,
            })
        );
        assert_eq!(collect.frame, gradient());
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
        assert_eq!(decode(&encoded), Err(ProtocolError::LevelOutOfRange(64)));
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(decode(&[9, 0, 0, 0, 0]), Err(ProtocolError::UnknownKind(9)));
    }

    #[test]
    fn rejects_the_rgb888_protocol() {
        assert_eq!(
            decode(&[1, 0x00, 0x30, 0, 0]),
            Err(ProtocolError::UnknownKind(1))
        );
    }

    #[test]
    fn rejects_wrong_length_before_reading_the_payload() {
        assert_eq!(
            decode(&[3, 0xff, 0xff, 0xff, 0x7f]),
            Err(ProtocolError::WrongLength {
                kind: 3,
                len: 0x7fff_ffff
            })
        );
    }

    #[test]
    fn rejects_out_of_range_brightness() {
        assert_eq!(
            decode(&[4, 1, 0, 0, 0, 101]),
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
