//! Server-to-panel stream: a sequence of `[kind: u8][length: u32 LE][payload]`
//! messages over one TCP connection.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::color::{Brightness, GammaExponent, Rgb, Tuning, WhiteBalance};

/// Edge length of the panel in pixels.
pub const SIZE: usize = 64;

const KIND_FRAME: u8 = 1;
const KIND_TUNE: u8 = 2;
const HEADER_LEN: usize = 5;
const FRAME_LEN: usize = SIZE * SIZE * 3;
const TUNE_LEN: usize = 6;

/// A full panel image, indexed as `pixels[y][x]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub pixels: Box<[[Rgb; SIZE]; SIZE]>,
}

impl Frame {
    pub fn black() -> Self {
        // Built row by row on the heap; `Box::new` of the whole array would
        // first place 12 KiB on the stack, which is most of a microcontroller's
        let rows = alloc::vec![[Rgb::default(); SIZE]; SIZE];
        let pixels = rows
            .into_boxed_slice()
            .try_into()
            .expect("SIZE rows were allocated");
        Frame { pixels }
    }

    /// `rgb` holds `SIZE * SIZE` row-major RGB triplets.
    fn from_rgb(rgb: &[u8]) -> Self {
        let rows: Vec<[Rgb; SIZE]> = rgb
            .chunks_exact(SIZE * 3)
            .map(|row| {
                core::array::from_fn(|x| Rgb {
                    r: row[x * 3],
                    g: row[x * 3 + 1],
                    b: row[x * 3 + 2],
                })
            })
            .collect();
        let pixels = rows
            .into_boxed_slice()
            .try_into()
            .expect("FRAME_LEN bytes hold exactly SIZE rows");
        Frame { pixels }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Frame(Frame),
    Tune(Tuning),
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload) = match self {
            Message::Frame(frame) => {
                let rgb = frame
                    .pixels
                    .iter()
                    .flatten()
                    .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
                    .collect();
                (KIND_FRAME, rgb)
            }
            Message::Tune(tuning) => {
                let gamma = tuning.gamma.hundredths().to_le_bytes();
                let (red, green, blue) = tuning.white_balance.percentages();
                let payload = Vec::from([
                    tuning.brightness.percent(),
                    gamma[0],
                    gamma[1],
                    red,
                    green,
                    blue,
                ]);
                (KIND_TUNE, payload)
            }
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
    InvalidTuning,
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
        KIND_TUNE => TUNE_LEN,
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
        KIND_FRAME => Message::Frame(Frame::from_rgb(payload)),
        _ => Message::Tune(decode_tuning(payload).ok_or(ProtocolError::InvalidTuning)?),
    };
    Ok(Some((message, HEADER_LEN + len)))
}

fn decode_tuning(payload: &[u8]) -> Option<Tuning> {
    Some(Tuning {
        brightness: Brightness::new(payload[0])?,
        gamma: GammaExponent::from_hundredths(u16::from_le_bytes([payload[1], payload[2]]))?,
        white_balance: WhiteBalance::new(payload[3], payload[4], payload[5])?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn gradient() -> Frame {
        let mut frame = Frame::black();
        for (y, row) in frame.pixels.iter_mut().enumerate() {
            for (x, pixel) in row.iter_mut().enumerate() {
                *pixel = Rgb {
                    r: x as u8 * 4,
                    g: y as u8 * 4,
                    b: (x + y) as u8,
                };
            }
        }
        frame
    }

    fn tuning() -> Tuning {
        Tuning {
            brightness: Brightness::new(35).unwrap(),
            gamma: GammaExponent::from_hundredths(260).unwrap(),
            white_balance: WhiteBalance::new(100, 80, 65).unwrap(),
        }
    }

    #[test]
    fn round_trips_messages_split_at_every_byte() {
        let messages = vec![Message::Tune(tuning()), Message::Frame(gradient())];
        let stream: Vec<u8> = messages.iter().flat_map(Message::encode).collect();
        for cut in 0..stream.len() {
            let mut reader = MessageReader::default();
            let mut decoded = reader.feed(&stream[..cut]).unwrap();
            decoded.extend(reader.feed(&stream[cut..]).unwrap());
            assert_eq!(decoded, messages, "split at {cut}");
        }
    }

    #[test]
    fn tune_encoding_is_stable() {
        assert_eq!(
            Message::Tune(tuning()).encode(),
            vec![2, 6, 0, 0, 0, 35, 0x04, 0x01, 100, 80, 65]
        );
    }

    #[test]
    fn frame_payload_is_row_major_rgb() {
        let encoded = Message::Frame(gradient()).encode();
        assert_eq!(&encoded[..HEADER_LEN], &[1, 0x00, 0x30, 0, 0]);
        // pixel (x=2, y=1)
        let offset = HEADER_LEN + (SIZE + 2) * 3;
        assert_eq!(&encoded[offset..offset + 3], &[8, 4, 3]);
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
    fn rejects_wrong_length_before_buffering_payload() {
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.feed(&[1, 0xff, 0xff, 0xff, 0x7f]),
            Err(ProtocolError::WrongLength {
                kind: 1,
                len: 0x7fff_ffff
            })
        );
    }

    #[test]
    fn rejects_out_of_range_tuning() {
        let mut reader = MessageReader::default();
        assert_eq!(
            reader.feed(&[2, 6, 0, 0, 0, 101, 220, 0, 100, 100, 100]),
            Err(ProtocolError::InvalidTuning)
        );
    }
}
