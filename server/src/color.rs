use std::ops::RangeInclusive;

use album_art_core::protocol::{Brightness, Level, PanelColor, PanelFrame};

use crate::frame::Frame;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Exponent of the sRGB-to-linear curve applied before driving the LEDs, in
/// hundredths so it has an exact integer form on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GammaExponent(u16);

impl GammaExponent {
    const RANGE: RangeInclusive<u16> = 100..=400;

    pub fn from_hundredths(hundredths: u16) -> Option<Self> {
        Self::RANGE
            .contains(&hundredths)
            .then_some(GammaExponent(hundredths))
    }

    pub fn hundredths(self) -> u16 {
        self.0
    }

    pub fn value(self) -> f32 {
        f32::from(self.0) / 100.0
    }
}

/// Per-channel output scale in percent, to compensate for the panel's LEDs
/// not being equally bright.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WhiteBalance {
    red: u8,
    green: u8,
    blue: u8,
}

impl WhiteBalance {
    pub fn new(red: u8, green: u8, blue: u8) -> Option<Self> {
        if red > 100 || green > 100 || blue > 100 {
            return None;
        }
        Some(WhiteBalance { red, green, blue })
    }

    pub fn percentages(self) -> (u8, u8, u8) {
        (self.red, self.green, self.blue)
    }
}

/// Everything about how the panel renders, as opposed to what it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    pub brightness: Brightness,
    pub gamma: GammaExponent,
    pub white_balance: WhiteBalance,
}

impl Tuning {
    pub const DEFAULT: Tuning = Tuning {
        brightness: match Brightness::new(20) {
            Some(brightness) => brightness,
            None => panic!("20% is a valid brightness"),
        },
        gamma: GammaExponent(235),
        white_balance: WhiteBalance {
            red: 100,
            green: 100,
            blue: 90,
        },
    };
}

/// Gamma, white balance and the panel's few levels per channel.
///
/// Rounding each channel on its own shifts hue near black: a dim warm yellow
/// keeps its red at the lowest level after its slightly weaker green has
/// rounded to zero, and shows as red. Each pixel instead takes whichever
/// nearby level combination looks closest to it.
pub struct ColorCorrection {
    /// Light output of every sRGB value, per channel, as a fraction of that
    /// LED's full output.
    linear: [[f32; 256]; 3],
}

impl ColorCorrection {
    pub fn new(gamma: GammaExponent, white_balance: WhiteBalance) -> Self {
        let exponent = gamma.value();
        let (red, green, blue) = white_balance.percentages();
        let table = |percent: u8| -> [f32; 256] {
            std::array::from_fn(|value| {
                (value as f32 / 255.0).powf(exponent) * f32::from(percent) / 100.0
            })
        };
        ColorCorrection {
            linear: [table(red), table(green), table(blue)],
        }
    }

    pub fn apply(&self, frame: &Frame) -> PanelFrame {
        let mut panel = PanelFrame::black();
        for (panel_row, row) in panel.pixels.iter_mut().zip(frame.pixels.iter()) {
            for (panel_color, &color) in panel_row.iter_mut().zip(row) {
                *panel_color = self.quantize(color);
            }
        }
        panel
    }

    /// Picks, among rounding each channel down or up, the combination
    /// closest to the wanted light in CIELAB, where distance follows how
    /// different colours look.
    fn quantize(&self, color: Rgb) -> PanelColor {
        let max = f32::from(Level::MAX.value());
        let values = [color.r, color.g, color.b];
        let wanted: [f32; 3] =
            std::array::from_fn(|channel| self.linear[channel][usize::from(values[channel])]);
        let target = lab(wanted);
        let bounds = wanted.map(|light| {
            let scaled = light * max;
            (
                scaled.floor() as u8,
                (scaled.ceil() as u8).min(Level::MAX.value()),
            )
        });
        let (_, best) = (0..8u8)
            .map(|choice| {
                let levels: [u8; 3] = std::array::from_fn(|channel| {
                    let (down, up) = bounds[channel];
                    if choice & (1 << channel) == 0 {
                        down
                    } else {
                        up
                    }
                });
                let shown = lab(levels.map(|level| f32::from(level) / max));
                (squared_distance(shown, target), levels)
            })
            .min_by(|(a, _), (b, _)| a.total_cmp(b))
            .expect("there are eight candidates");
        PanelColor {
            r: Level::new(best[0]),
            g: Level::new(best[1]),
            b: Level::new(best[2]),
        }
    }
}

/// CIELAB (D65) from linear-light RGB. Its lightness is linear near black,
/// so a barely lit pixel stays close to black; perceptual spaces built on a
/// plain cube root, such as OKLab, rate black far from any dark colour and
/// would light a single LED instead.
fn lab([r, g, b]: [f32; 3]) -> [f32; 3] {
    let x = (0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b) / 0.950_47;
    let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175 * b;
    let z = (0.019_333_9 * r + 0.119_192 * g + 0.950_304_1 * b) / 1.088_83;
    let f = |t: f32| {
        if t > 216.0 / 24_389.0 {
            t.cbrt()
        } else {
            (24_389.0 / 27.0 * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn squared_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter().zip(&b).map(|(x, y)| (x - y) * (x - y)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gamma(hundredths: u16) -> GammaExponent {
        GammaExponent::from_hundredths(hundredths).unwrap()
    }

    fn grey(value: u8) -> Rgb {
        Rgb {
            r: value,
            g: value,
            b: value,
        }
    }

    fn levels(color: PanelColor) -> [u8; 3] {
        [color.r.value(), color.g.value(), color.b.value()]
    }

    fn neutral(hundredths: u16) -> ColorCorrection {
        ColorCorrection::new(gamma(hundredths), WhiteBalance::new(100, 100, 100).unwrap())
    }

    #[test]
    fn keeps_black_and_white_at_the_ends_of_the_scale() {
        let correction = neutral(220);
        assert_eq!(levels(correction.quantize(grey(0))), [0, 0, 0]);
        assert_eq!(levels(correction.quantize(grey(255))), [255, 255, 255]);
    }

    #[test]
    fn rounds_to_the_nearest_level_instead_of_truncating() {
        let correction = neutral(220);
        // sRGB 18 is 0.29% linear, three quarters of the lowest level
        assert_eq!(levels(correction.quantize(grey(18))), [1, 1, 1]);
        assert_eq!(levels(correction.quantize(grey(10))), [0, 0, 0]);
    }

    #[test]
    fn greys_stay_neutral() {
        let correction = neutral(235);
        for value in 0..=255 {
            let [r, g, b] = levels(correction.quantize(grey(value)));
            assert!(r == g && g == b, "grey {value} became {r}/{g}/{b}");
        }
    }

    #[test]
    fn dim_yellow_turns_red_by_at_most_the_lowest_level() {
        // Sampled from a cover whose yellow-to-black gradient showed a red
        // edge. Just above black, a lone lowest red level is genuinely the
        // nearest colour; a brighter red-only pixel is the band that showed.
        let correction = ColorCorrection::new(gamma(235), WhiteBalance::new(100, 100, 90).unwrap());
        for green_share in [0.76, 0.8, 0.86, 0.9] {
            for value in 1..=255u8 {
                let yellow = Rgb {
                    r: value,
                    g: (f32::from(value) * green_share).round() as u8,
                    b: (f32::from(value) * 0.19).round() as u8,
                };
                let [r, g, _] = levels(correction.quantize(yellow));
                assert!(r <= 1 || g > 0, "{yellow:?} became red only: {r}/{g}");
            }
        }
    }

    #[test]
    fn white_balance_scales_each_channel() {
        let balance = WhiteBalance::new(100, 80, 50).unwrap();
        let correction = ColorCorrection::new(gamma(220), balance);
        let [r, g, b] = levels(correction.quantize(grey(255)));
        assert_eq!((r, g), (255, 204));
        assert!((127..=128).contains(&b), "blue {b}");
    }

    #[test]
    fn linear_gamma_keeps_proportions() {
        let correction = neutral(100);
        assert_eq!(levels(correction.quantize(grey(128))), [128, 128, 128]);
    }

    #[test]
    fn rejects_out_of_range_settings() {
        assert_eq!(GammaExponent::from_hundredths(99), None);
        assert_eq!(GammaExponent::from_hundredths(401), None);
        assert_eq!(WhiteBalance::new(100, 101, 0), None);
        assert_eq!(Brightness::new(101), None);
    }
}
