use album_art_core::color::Rgb;
use album_art_core::protocol::{Frame, SIZE};
use image::imageops::FilterType;

/// Decodes a cover and scales it to the panel, averaging every source pixel
/// so JPEG noise in the original does not survive into the 64x64 result.
pub fn to_frame(encoded: &[u8]) -> Result<Frame, image::ImageError> {
    let cover = image::load_from_memory(encoded)?;
    let scaled = cover
        .resize_to_fill(SIZE as u32, SIZE as u32, FilterType::Lanczos3)
        .to_rgb8();
    let mut frame = Frame::black();
    for (x, y, pixel) in scaled.enumerate_pixels() {
        let [r, g, b] = pixel.0;
        frame.pixels[y as usize][x as usize] = Rgb { r, g, b };
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage};

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
}
