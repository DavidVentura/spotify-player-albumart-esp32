use core::ops::RangeInclusive;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Panel brightness as a percentage of full output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brightness(u8);

impl Brightness {
    pub fn new(percent: u8) -> Option<Self> {
        (percent <= 100).then_some(Brightness(percent))
    }

    pub fn percent(self) -> u8 {
        self.0
    }

    /// Fraction of full output in the 0..=255 scale the framebuffer uses.
    pub fn duty(self) -> u8 {
        ((u16::from(self.0) * 255 + 50) / 100) as u8
    }
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

/// Everything about how the panel renders, as opposed to what it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    pub brightness: Brightness,
    pub gamma: GammaExponent,
    pub white_balance: WhiteBalance,
}

impl Tuning {
    pub const DEFAULT: Tuning = Tuning {
        brightness: Brightness(20),
        gamma: GammaExponent(235),
        white_balance: WhiteBalance {
            red: 100,
            green: 100,
            blue: 90,
        },
    };
}

/// Gamma, white balance and quantization to the panel's bit depth, folded
/// into one lookup table per channel.
pub struct ColorCorrection {
    red: [u8; 256],
    green: [u8; 256],
    blue: [u8; 256],
}

impl ColorCorrection {
    /// `panel_bits` is how many of the most significant bits the panel
    /// shows. Values are rounded to the nearest level it can show, because
    /// the framebuffer drops the low bits, which would truncate dark tones to
    /// black.
    pub fn new(gamma: GammaExponent, white_balance: WhiteBalance, panel_bits: u32) -> Self {
        assert!((1..=8).contains(&panel_bits), "panels show 1 to 8 bits");
        let exponent = gamma.value();
        let max_level = ((1u32 << panel_bits) - 1) as f32;
        let table = |percent: u8| -> [u8; 256] {
            core::array::from_fn(|value| {
                let linear = libm::powf(value as f32 / 255.0, exponent);
                let scaled = linear * f32::from(percent) / 100.0;
                let level = libm::roundf(scaled * max_level) as u32;
                (level << (8 - panel_bits)) as u8
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

    fn gamma(hundredths: u16) -> GammaExponent {
        GammaExponent::from_hundredths(hundredths).unwrap()
    }

    const WHITE: Rgb = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };

    fn grey(level: u8) -> Rgb {
        Rgb {
            r: level,
            g: level,
            b: level,
        }
    }

    #[test]
    fn full_depth_keeps_endpoints_and_darkens_midtones() {
        let correction = ColorCorrection::new(gamma(220), WhiteBalance::NEUTRAL, 8);
        assert_eq!(correction.apply(grey(0)), grey(0));
        assert_eq!(correction.apply(WHITE), WHITE);
        assert_eq!(correction.apply(grey(128)), grey(56));
    }

    #[test]
    fn rounds_to_nearest_panel_level_instead_of_truncating() {
        let correction = ColorCorrection::new(gamma(220), WhiteBalance::NEUTRAL, 6);
        // sRGB 34 is 1.2% linear, three quarters of a 6-bit step; truncating
        // the 8-bit value would show it as black
        assert_eq!(correction.apply(grey(34)), grey(1 << 2));
        assert_eq!(correction.apply(grey(20)), grey(0));
        // Full scale lands on the top 6-bit level
        assert_eq!(correction.apply(WHITE), grey(63 << 2));
    }

    #[test]
    fn white_balance_scales_each_channel() {
        let balance = WhiteBalance::new(100, 80, 50).unwrap();
        let correction = ColorCorrection::new(gamma(220), balance, 8);
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
    fn linear_gamma_is_identity_at_full_depth() {
        let linear = GammaExponent::from_hundredths(100).unwrap();
        let correction = ColorCorrection::new(linear, WhiteBalance::NEUTRAL, 8);
        assert_eq!(correction.apply(grey(128)), grey(128));
    }

    #[test]
    fn rejects_out_of_range_settings() {
        assert_eq!(GammaExponent::from_hundredths(99), None);
        assert_eq!(GammaExponent::from_hundredths(401), None);
        assert_eq!(WhiteBalance::new(100, 101, 0), None);
        assert_eq!(Brightness::new(101), None);
    }

    #[test]
    fn duty_maps_percent_to_full_scale() {
        let duty = |percent| Brightness::new(percent).unwrap().duty();
        assert_eq!(duty(0), 0);
        assert_eq!(duty(20), 51);
        assert_eq!(duty(50), 128);
        assert_eq!(duty(100), 255);
    }
}
