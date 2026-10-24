use core::ops::RangeInclusive;

use crate::thumbnail::Rgb;

/// Exponent of the sRGB-to-linear curve applied before driving the LEDs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GammaExponent(f32);

impl GammaExponent {
    pub const DEFAULT: GammaExponent = GammaExponent(2.2);
    const RANGE: RangeInclusive<f32> = 1.0..=4.0;

    pub fn new(exponent: f32) -> Option<Self> {
        Self::RANGE
            .contains(&exponent)
            .then_some(GammaExponent(exponent))
    }

    pub fn value(self) -> f32 {
        self.0
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
    pub const NEUTRAL: WhiteBalance = WhiteBalance {
        red: 100,
        green: 100,
        blue: 100,
    };

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

/// Gamma and white balance folded into one lookup table per channel.
pub struct ColorCorrection {
    red: [u8; 256],
    green: [u8; 256],
    blue: [u8; 256],
}

impl ColorCorrection {
    pub fn new(gamma: GammaExponent, white_balance: WhiteBalance) -> Self {
        let table = |percent: u8| -> [u8; 256] {
            core::array::from_fn(|value| {
                let linear = libm::powf(value as f32 / 255.0, gamma.0);
                libm::roundf(linear * 255.0 * f32::from(percent) / 100.0) as u8
            })
        };
        ColorCorrection {
            red: table(white_balance.red),
            green: table(white_balance.green),
            blue: table(white_balance.blue),
        }
    }

    pub fn apply(&self, color: Rgb) -> Rgb {
        Rgb {
            r: self.red[color.r as usize],
            g: self.green[color.g as usize],
            b: self.blue[color.b as usize],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: Rgb = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    const GREY: Rgb = Rgb {
        r: 128,
        g: 128,
        b: 128,
    };

    #[test]
    fn neutral_gamma_keeps_endpoints_and_darkens_midtones() {
        let correction = ColorCorrection::new(GammaExponent::DEFAULT, WhiteBalance::NEUTRAL);
        assert_eq!(correction.apply(Rgb::default()), Rgb::default());
        assert_eq!(correction.apply(WHITE), WHITE);
        assert_eq!(
            correction.apply(GREY),
            Rgb {
                r: 56,
                g: 56,
                b: 56
            }
        );
    }

    #[test]
    fn white_balance_scales_each_channel() {
        let balance = WhiteBalance::new(100, 80, 50).unwrap();
        let correction = ColorCorrection::new(GammaExponent::DEFAULT, balance);
        assert_eq!(
            correction.apply(WHITE),
            Rgb {
                r: 255,
                g: 204,
                b: 128
            }
        );
    }

    #[test]
    fn linear_gamma_is_identity() {
        let correction =
            ColorCorrection::new(GammaExponent::new(1.0).unwrap(), WhiteBalance::NEUTRAL);
        assert_eq!(correction.apply(GREY), GREY);
    }

    #[test]
    fn rejects_out_of_range_settings() {
        assert_eq!(GammaExponent::new(0.5), None);
        assert_eq!(GammaExponent::new(4.5), None);
        assert_eq!(GammaExponent::new(f32::NAN), None);
        assert_eq!(WhiteBalance::new(100, 101, 0), None);
    }
}
