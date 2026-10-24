use alloc::boxed::Box;
use alloc::vec::Vec;

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;
use zune_jpeg::errors::DecodeErrors;

/// Edge length of both the panel and the smallest cover Spotify serves.
pub const SIZE: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A decoded cover, indexed as `pixels[y][x]`.
pub struct Thumbnail {
    pub pixels: Box<[[Rgb; SIZE]; SIZE]>,
}

#[derive(Debug)]
pub enum DecodeError {
    Jpeg(DecodeErrors),
    WrongSize { width: usize, height: usize },
}

pub fn decode_thumbnail(jpeg: &[u8]) -> Result<Thumbnail, DecodeError> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    // The decoder holds ~28 KiB of Huffman tables inline; on a microcontroller
    // that does not fit on the stack
    let mut decoder = Box::new(JpegDecoder::new_with_options(ZCursor::new(jpeg), options));
    decoder.decode_headers().map_err(DecodeError::Jpeg)?;
    let (width, height) = decoder
        .dimensions()
        .expect("dimensions are known once headers are decoded");
    if (width, height) != (SIZE, SIZE) {
        return Err(DecodeError::WrongSize { width, height });
    }
    let rgb = decoder.decode().map_err(DecodeError::Jpeg)?;

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
        .expect("a SIZE x SIZE RGB decode yields SIZE rows");
    Ok(Thumbnail { pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: Rgb, expected: Rgb) {
        let close = |a: u8, b: u8| a.abs_diff(b) <= 8;
        assert!(
            close(actual.r, expected.r)
                && close(actual.g, expected.g)
                && close(actual.b, expected.b),
            "{actual:?} is not close to {expected:?}"
        );
    }

    const RED: Rgb = Rgb { r: 255, g: 0, b: 0 };
    const BLUE: Rgb = Rgb { r: 0, g: 0, b: 255 };

    #[test]
    fn decodes_baseline_jpeg() {
        let thumbnail = decode_thumbnail(include_bytes!("../tests/fixtures/split-64.jpg")).unwrap();
        assert_close(thumbnail.pixels[10][5], RED);
        assert_close(thumbnail.pixels[60][58], BLUE);
    }

    #[test]
    fn decodes_progressive_jpeg() {
        let jpeg = include_bytes!("../tests/fixtures/split-64-progressive.jpg");
        let thumbnail = decode_thumbnail(jpeg).unwrap();
        assert_close(thumbnail.pixels[0][0], RED);
        assert_close(thumbnail.pixels[63][63], BLUE);
    }

    #[test]
    fn rejects_other_sizes() {
        let result = decode_thumbnail(include_bytes!("../tests/fixtures/green-32.jpg"));
        assert!(matches!(
            result,
            Err(DecodeError::WrongSize {
                width: 32,
                height: 32
            })
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            decode_thumbnail(b"<html>not a jpeg</html>"),
            Err(DecodeError::Jpeg(_))
        ));
    }
}
