use alloc::vec::Vec;

use crate::color::{GammaExponent, WhiteBalance};

/// Commands are short; anything longer cannot be valid.
const MAX_LINE: usize = 32;

/// Panel brightness as a percentage of full output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brightness(u8);

impl Brightness {
    pub fn percent(self) -> u8 {
        self.0
    }

    /// Fraction of full output in the 0..=255 scale the framebuffer uses.
    pub fn duty(self) -> u8 {
        ((u16::from(self.0) * 255 + 50) / 100) as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Brightness(Brightness),
    Gamma(GammaExponent),
    WhiteBalance(WhiteBalance),
}

impl Command {
    /// Parses one control line; anything that is not a valid command is
    /// `None`:
    ///
    /// - `<percent>` sets the brightness
    /// - `gamma <exponent>` sets the gamma curve, 1.0 to 4.0
    /// - `wb <red> <green> <blue>` sets per-channel output in percent
    pub fn parse(line: &[u8]) -> Option<Command> {
        let text = core::str::from_utf8(line).ok()?;
        let mut words = text.split_ascii_whitespace();
        let command = match (words.next()?, words.next(), words.next(), words.next()) {
            ("gamma", Some(exponent), None, None) => {
                Command::Gamma(GammaExponent::new(parse_decimal(exponent)?)?)
            }
            ("wb", Some(red), Some(green), Some(blue)) => Command::WhiteBalance(WhiteBalance::new(
                parse_percent(red)?,
                parse_percent(green)?,
                parse_percent(blue)?,
            )?),
            (percent, None, None, None) => Command::Brightness(Brightness(parse_percent(percent)?)),
            _ => return None,
        };
        if words.next().is_some() {
            return None;
        }
        Some(command)
    }
}

/// Everything the control server can adjust.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    pub brightness: Brightness,
    pub gamma: GammaExponent,
    pub white_balance: WhiteBalance,
}

impl Tuning {
    pub const DEFAULT: Tuning = Tuning {
        brightness: Brightness(20),
        gamma: GammaExponent::DEFAULT,
        white_balance: WhiteBalance::NEUTRAL,
    };

    pub fn with(self, command: Command) -> Tuning {
        match command {
            Command::Brightness(brightness) => Tuning { brightness, ..self },
            Command::Gamma(gamma) => Tuning { gamma, ..self },
            Command::WhiteBalance(white_balance) => Tuning {
                white_balance,
                ..self
            },
        }
    }
}

fn parse_percent(word: &str) -> Option<u8> {
    if !word.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let percent: u8 = word.parse().ok()?;
    (percent <= 100).then_some(percent)
}

/// `f32::from_str` also accepts signs, exponents, `inf` and `NaN`.
fn parse_decimal(word: &str) -> Option<f32> {
    if !word.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    word.parse().ok()
}

/// Splits a byte stream into newline-terminated lines.
#[derive(Default)]
pub struct LineSplitter {
    current: Vec<u8>,
    overlong: bool,
}

/// One line from the stream, with overlong lines kept distinct so they can
/// never be mistaken for a truncated valid command.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Text(Vec<u8>),
    Overlong,
}

impl LineSplitter {
    /// Returns the lines completed by `data`.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Line> {
        let mut lines = Vec::new();
        for &byte in data {
            if byte == b'\n' {
                lines.push(self.take());
                continue;
            }
            if self.current.len() == MAX_LINE {
                self.overlong = true;
                continue;
            }
            self.current.push(byte);
        }
        lines
    }

    /// Returns the unterminated last line when the stream ends.
    pub fn finish(mut self) -> Option<Line> {
        if self.current.is_empty() && !self.overlong {
            return None;
        }
        Some(self.take())
    }

    fn take(&mut self) -> Line {
        let line = core::mem::take(&mut self.current);
        if core::mem::take(&mut self.overlong) {
            return Line::Overlong;
        }
        Line::Text(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn text(line: &str) -> Line {
        Line::Text(line.as_bytes().to_vec())
    }

    fn parse(line: &str) -> Option<Command> {
        Command::parse(line.as_bytes())
    }

    #[test]
    fn parses_brightness() {
        assert_eq!(parse("0"), Some(Command::Brightness(Brightness(0))));
        assert_eq!(parse("100"), Some(Command::Brightness(Brightness(100))));
        assert_eq!(parse(" 42\r"), Some(Command::Brightness(Brightness(42))));
        assert_eq!(parse("007"), Some(Command::Brightness(Brightness(7))));
    }

    #[test]
    fn parses_gamma() {
        assert_eq!(
            parse("gamma 2.6"),
            Some(Command::Gamma(GammaExponent::new(2.6).unwrap()))
        );
        assert_eq!(
            parse("gamma 3"),
            Some(Command::Gamma(GammaExponent::new(3.0).unwrap()))
        );
    }

    #[test]
    fn parses_white_balance() {
        assert_eq!(
            parse("wb 100 80 65\r"),
            Some(Command::WhiteBalance(
                WhiteBalance::new(100, 80, 65).unwrap()
            ))
        );
    }

    #[test]
    fn ignores_non_commands() {
        for line in [
            "",
            "   ",
            "101",
            "256",
            "-1",
            "+5",
            "5%",
            "5.5",
            "five",
            "1 2",
            "gamma",
            "gamma 0.5",
            "gamma 9",
            "gamma inf",
            "gamma NaN",
            "gamma 1e0",
            "gamma -2",
            "gamma 2 3",
            "wb 100 80",
            "wb 100 80 65 1",
            "wb 101 0 0",
            "WB 1 2 3",
        ] {
            assert_eq!(parse(line), None, "{line:?}");
        }
        assert_eq!(Command::parse(b"\xff"), None);
    }

    #[test]
    fn tuning_applies_one_setting_at_a_time() {
        let gamma = GammaExponent::new(2.6).unwrap();
        let tuning = Tuning::DEFAULT
            .with(Command::Gamma(gamma))
            .with(Command::Brightness(Brightness(70)));
        assert_eq!(
            tuning,
            Tuning {
                brightness: Brightness(70),
                gamma,
                ..Tuning::DEFAULT
            }
        );
    }

    #[test]
    fn duty_maps_percent_to_full_scale() {
        assert_eq!(Brightness(0).duty(), 0);
        assert_eq!(Brightness(20).duty(), 51);
        assert_eq!(Brightness(50).duty(), 128);
        assert_eq!(Brightness(100).duty(), 255);
    }

    #[test]
    fn splits_lines_across_reads() {
        let mut splitter = LineSplitter::default();
        assert_eq!(splitter.feed(b"1"), vec![]);
        assert_eq!(splitter.feed(b"0\n20\n3"), vec![text("10"), text("20")]);
        assert_eq!(splitter.finish(), Some(text("3")));
    }

    #[test]
    fn finish_without_pending_bytes_yields_nothing() {
        let mut splitter = LineSplitter::default();
        assert_eq!(splitter.feed(b"5\n"), vec![text("5")]);
        assert_eq!(splitter.finish(), None);
    }

    #[test]
    fn overlong_lines_are_flagged_and_do_not_leak_into_the_next() {
        let mut splitter = LineSplitter::default();
        let long = [b'1'; MAX_LINE + 5];
        assert_eq!(splitter.feed(&long), vec![]);
        assert_eq!(splitter.feed(b"\n7\n"), vec![Line::Overlong, text("7")]);
    }
}
