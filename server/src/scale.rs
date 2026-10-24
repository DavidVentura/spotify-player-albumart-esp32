//! Fits arbitrary images and GIFs to the panel.

use std::io::Cursor;
use std::time::Duration;

use album_art_core::color::Rgb;
use album_art_core::protocol::{Frame, SIZE};
use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageError, RgbImage};

/// GIFs often declare a zero delay; browsers show those at 10 fps, and so do we.
const MIN_FRAME_DELAY: Duration = Duration::from_millis(20);
const DEFAULT_FRAME_DELAY: Duration = Duration::from_millis(100);

/// Decodes any still image and scales it to the panel.
pub fn to_frame(encoded: &[u8]) -> Result<Frame, ImageError> {
    Ok(fit(image::load_from_memory(encoded)?))
}

/// Decodes every frame of a GIF, each with how long it stays on screen.
pub fn to_animation(gif: &[u8]) -> Result<Vec<(Frame, Duration)>, ImageError> {
    let decoder = GifDecoder::new(Cursor::new(gif))?;
    decoder
        .into_frames()
        .map(|frame| {
            let frame = frame?;
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let delay = Duration::from_micros(u64::from(numerator) * 1000 / u64::from(denominator));
            let delay = if delay < MIN_FRAME_DELAY {
                DEFAULT_FRAME_DELAY
            } else {
                delay
            };
            Ok((fit(DynamicImage::ImageRgba8(frame.into_buffer())), delay))
        })
        .collect()
}

/// Flattens transparency onto black (the panel's "off") at full resolution,
/// then averages every source pixel into the 64x64 result so noise in the
/// original does not survive.
fn fit(image: DynamicImage) -> Frame {
    let rgba = image.to_rgba8();
    let flattened = RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let over_black = |channel: u8| ((u16::from(channel) * u16::from(a) + 127) / 255) as u8;
        [over_black(r), over_black(g), over_black(b)].into()
    });
    let scaled = DynamicImage::ImageRgb8(flattened)
        .resize_to_fill(SIZE as u32, SIZE as u32, FilterType::Lanczos3)
        .to_rgb8();
    let mut frame = Frame::black();
    for (x, y, pixel) in scaled.enumerate_pixels() {
        let [r, g, b] = pixel.0;
        frame.pixels[y as usize][x as usize] = Rgb { r, g, b };
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::codecs::jpeg::JpegEncoder;
    use image::{Delay, ImageEncoder, RgbaImage};

    const RED: [u8; 3] = [255, 0, 0];
    const BLUE: [u8; 3] = [0, 0, 255];

    fn jpeg(image: &RgbImage) -> Vec<u8> {
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, 95)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        encoded
    }

    fn assert_close(actual: Rgb, expected: [u8; 3]) {
        let close = |a: u8, b: u8| a.abs_diff(b) <= 8;
        assert!(
            close(actual.r, expected[0])
                && close(actual.g, expected[1])
                && close(actual.b, expected[2]),
            "{actual:?} is not close to {expected:?}"
        );
    }

    #[test]
    fn scales_large_cover_down_to_the_panel() {
        let cover = RgbImage::from_fn(
            640,
            640,
            |x, _| if x < 320 { RED.into() } else { BLUE.into() },
        );
        let frame = to_frame(&jpeg(&cover)).unwrap();
        assert_close(frame.pixels[10][5], RED);
        assert_close(frame.pixels[60][58], BLUE);
    }

    #[test]
    fn averages_fine_noise_away() {
        // A checkerboard finer than one panel pixel must come out uniform grey
        let cover = RgbImage::from_fn(640, 640, |x, y| {
            if (x + y) % 2 == 0 {
                [255, 255, 255].into()
            } else {
                [0, 0, 0].into()
            }
        });
        let frame = to_frame(&jpeg(&cover)).unwrap();
        let first = frame.pixels[32][32];
        assert!(first.r.abs_diff(first.g) <= 2 && first.g.abs_diff(first.b) <= 2);
        for row in frame.pixels.iter() {
            for pixel in row {
                assert!(pixel.r.abs_diff(first.r) <= 6, "{pixel:?} vs {first:?}");
            }
        }
    }

    #[test]
    fn crops_non_square_covers_to_the_centre() {
        // The centre crop is x 150..750; blue sits clear of it so the
        // resampling filter at the crop edge only ever sees red
        let cover = RgbImage::from_fn(900, 600, |x, _| {
            if (100..800).contains(&x) {
                RED.into()
            } else {
                BLUE.into()
            }
        });
        let frame = to_frame(&jpeg(&cover)).unwrap();
        assert_close(frame.pixels[32][0], RED);
        assert_close(frame.pixels[32][63], RED);
    }

    #[test]
    fn rejects_garbage() {
        assert!(to_frame(b"<html>not an image</html>").is_err());
    }

    fn gif(frames: &[(RgbaImage, u32)]) -> Vec<u8> {
        let mut encoded = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut encoded);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for (image, delay_ms) in frames {
                let delay = Delay::from_numer_denom_ms(*delay_ms, 1);
                encoder
                    .encode_frame(image::Frame::from_parts(image.clone(), 0, 0, delay))
                    .unwrap();
            }
        }
        encoded
    }

    fn solid(color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(32, 32, color.into())
    }

    #[test]
    fn decodes_every_gif_frame_with_its_delay() {
        let encoded =
            gif(&[([255, 0, 0, 255], 40), ([0, 0, 255, 255], 70)].map(|(c, d)| (solid(c), d)));
        let frames = to_animation(&encoded).unwrap();
        assert_eq!(frames.len(), 2);
        assert_close(frames[0].0.pixels[10][10], RED);
        assert_eq!(frames[0].1, Duration::from_millis(40));
        assert_close(frames[1].0.pixels[10][10], BLUE);
        assert_eq!(frames[1].1, Duration::from_millis(70));
    }

    #[test]
    fn zero_delay_frames_play_at_ten_fps() {
        let encoded = gif(&[(solid([255, 0, 0, 255]), 0)]);
        assert_eq!(to_animation(&encoded).unwrap()[0].1, DEFAULT_FRAME_DELAY);
    }

    #[test]
    fn transparency_becomes_black() {
        let half = RgbaImage::from_fn(64, 64, |x, _| {
            if x < 32 {
                [255, 255, 255, 0].into()
            } else {
                [255, 0, 0, 255].into()
            }
        });
        let mut png = Vec::new();
        half.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let frame = to_frame(&png).unwrap();
        assert_close(frame.pixels[32][5], [0, 0, 0]);
        assert_close(frame.pixels[32][60], RED);
    }

    #[test]
    fn still_image_is_not_an_animation() {
        assert!(to_animation(&jpeg(&RgbImage::new(8, 8))).is_err());
    }
}
