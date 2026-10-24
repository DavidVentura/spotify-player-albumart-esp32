//! Fits arbitrary images and GIFs to the panel.

use std::io::Cursor;
use std::time::Duration;

use album_art_core::color::Rgb;
use album_art_core::protocol::{Frame, SIZE};
use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, DynamicImage, ImageError};

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

/// Crops the centre square and averages every source pixel it covers into
/// the 64x64 result, weighted by how much of each panel pixel it covers.
/// Unlike windowed-sinc filters, an average cannot overshoot next to hard
/// edges, which the panel's coarse dark levels would show as stray specks.
/// Averaging and the flattening of transparency onto black (the panel's
/// "off") happen in linear light, where mixing light is physically additive.
fn fit(image: DynamicImage) -> Frame {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let side = f64::from(width.min(height));
    let columns = coverage(f64::from(width - width.min(height)) / 2.0, side);
    let rows = coverage(f64::from(height - width.min(height)) / 2.0, side);
    let to_linear: [f32; 256] = core::array::from_fn(|value| srgb_to_linear(value as u8));

    // Separable: average each source row into panel columns first, so every
    // source pixel is read once
    let mut row_averages = vec![[[0f32; 3]; SIZE]; height as usize];
    for (y, averages) in row_averages.iter_mut().enumerate() {
        for (average, column) in averages.iter_mut().zip(&columns) {
            for &(x, weight) in column {
                let [r, g, b, a] = rgba.get_pixel(x, y as u32).0;
                let opacity = f32::from(a) / 255.0;
                for (sum, channel) in average.iter_mut().zip([r, g, b]) {
                    *sum += to_linear[usize::from(channel)] * opacity * weight;
                }
            }
        }
    }

    let mut frame = Frame::black();
    for (frame_row, row) in frame.pixels.iter_mut().zip(&rows) {
        for (x, pixel) in frame_row.iter_mut().enumerate() {
            let mut linear = [0f32; 3];
            for &(y, weight) in row {
                for (sum, value) in linear.iter_mut().zip(row_averages[y as usize][x]) {
                    *sum += value * weight;
                }
            }
            let [r, g, b] = linear.map(linear_to_srgb);
            *pixel = Rgb { r, g, b };
        }
    }
    frame
}

/// For each panel pixel along one axis, the source pixels it covers within
/// `[start, start + length)` and the fraction of the panel pixel each one
/// covers; the fractions of every panel pixel add up to one.
fn coverage(start: f64, length: f64) -> Vec<Vec<(u32, f32)>> {
    let step = length / SIZE as f64;
    (0..SIZE)
        .map(|index| {
            let from = start + index as f64 * step;
            let to = from + step;
            (from.floor() as u32..to.ceil() as u32)
                .map(|source| {
                    let overlap = to.min(f64::from(source + 1)) - from.max(f64::from(source));
                    (source, (overlap / step) as f32)
                })
                .filter(|&(_, weight)| weight > 0.0)
                .collect()
        })
        .collect()
}

fn srgb_to_linear(value: u8) -> f32 {
    let encoded = f32::from(value) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(linear: f32) -> u8 {
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::codecs::jpeg::JpegEncoder;
    use image::{Delay, ImageEncoder, RgbImage, RgbaImage};

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
        // The centre crop is x 150..750; blue sits clear of it so JPEG
        // ringing at the colour edge stays outside the crop
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

    fn png(image: &RgbImage) -> Vec<u8> {
        let mut encoded = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
            .unwrap();
        encoded
    }

    #[test]
    fn hard_edges_do_not_ring_into_black() {
        let cover = RgbImage::from_fn(640, 640, |x, _| {
            if x < 320 {
                [255, 255, 255].into()
            } else {
                [0, 0, 0].into()
            }
        });
        let frame = to_frame(&png(&cover)).unwrap();
        for row in frame.pixels.iter() {
            assert!(row[..32].iter().all(|&p| p
                == Rgb {
                    r: 255,
                    g: 255,
                    b: 255
                }));
            assert!(row[32..].iter().all(|&p| p == Rgb { r: 0, g: 0, b: 0 }));
        }
    }

    #[test]
    fn averages_in_linear_light() {
        // Half the light of white is sRGB 188, not the naive 128
        let checkerboard = RgbImage::from_fn(640, 640, |x, y| {
            if (x + y) % 2 == 0 {
                [255, 255, 255].into()
            } else {
                [0, 0, 0].into()
            }
        });
        let frame = to_frame(&png(&checkerboard)).unwrap();
        assert_eq!(
            frame.pixels[17][40],
            Rgb {
                r: 188,
                g: 188,
                b: 188
            }
        );
    }

    #[test]
    fn uneven_scales_keep_flat_colour_exact() {
        let flat = RgbImage::from_pixel(97, 61, [37, 140, 222].into());
        let frame = to_frame(&png(&flat)).unwrap();
        for row in frame.pixels.iter() {
            assert!(row.iter().all(|&p| p
                == Rgb {
                    r: 37,
                    g: 140,
                    b: 222
                }));
        }
    }

    #[test]
    fn small_images_scale_up_into_blocks() {
        let small = RgbImage::from_fn(8, 8, |x, y| {
            if (x + y) % 2 == 0 {
                RED.into()
            } else {
                BLUE.into()
            }
        });
        let frame = to_frame(&png(&small)).unwrap();
        let red = Rgb { r: 255, g: 0, b: 0 };
        let blue = Rgb { r: 0, g: 0, b: 255 };
        assert_eq!(frame.pixels[0][0], red);
        assert_eq!(frame.pixels[7][7], red);
        assert_eq!(frame.pixels[0][8], blue);
        assert_eq!(frame.pixels[63][56], red);
        assert_eq!(frame.pixels[63][48], blue);
    }

    #[test]
    fn coverage_weights_sum_to_one() {
        for (start, length) in [(0.0, 640.0), (19.5, 61.0), (0.0, 8.0), (3.0, 100.0)] {
            for weights in coverage(start, length) {
                let total: f32 = weights.iter().map(|&(_, weight)| weight).sum();
                assert!((total - 1.0).abs() < 1e-4, "{start} {length}: {total}");
            }
        }
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
