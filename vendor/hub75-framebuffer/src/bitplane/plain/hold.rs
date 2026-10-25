//! Row-major bitplane framebuffer that shifts each plane in once per row and
//! holds it lit for its bit-weight.
//!
//! The other bitplane layouts stream a plane once per unit of its weight, so
//! a refresh costs `COLS × NROWS × (2^PLANES − 1)` clocks: 8 planes on a
//! 64-column panel refresh at under 40 Hz. Here a plane is shown while the
//! next one shifts in, and planes that outlast a shift are held lit by
//! streaming a per-row run of lit words, so a row costs roughly
//! `Σ max(COLS, weight)` clocks and 8 planes refresh at several hundred Hz.
//!
//! One row's segments, with `Sk` shifting plane `k` in and `Hk` holding it:
//!
//! ```text
//! S0 H0 S1 H1 … S(n-1) H(n-1) dark
//! ```
//!
//! `Sk` (`k ≥ 1`) lights plane `k − 1` in its output-enable window; `S0` keeps
//! the previous row addressed and lights the remainder of its MSB, so the
//! address changes only once per row, right after `S0`'s latch.
//!
//! Brightness never shortens the planes relative to each other. It picks the
//! on-time unit (`2^k` units for plane `k`) and pads each row with dark words,
//! so every brightness keeps the full depth until the unit would drop below
//! one clock.
//!
//! The hold and dark words live in a [`Holds`] shared by every framebuffer,
//! since they depend only on the row address.

use embedded_graphics::pixelcolor::RgbColor;
use embedded_graphics::prelude::Point;

use crate::Color;
use crate::{map_row_index, slot_addresses};
use crate::{BcmLenReps, BcmSegment, FrameBuffer, BCM_SEQUENCE_CAPACITY};

use super::{map_index, Entry, LEAD_BLANK_DELAY, OE_ACTIVE, OE_BLANK, TRAIL_BLANK_DELAY};

const OE_MASK: u16 = 0b1_0000_0000;
const LATCH: u16 = 0b0010_0000;

/// Longest lit run a single hold descriptor streams; longer holds repeat it.
pub const HOLD_WORDS: usize = 128;
/// The most 16-bit words one DMA descriptor carries.
pub const DARK_WORDS: usize = 2046;
const DARK_REPS: usize = 2;
/// ESP32 DMA streams whole 32-bit words, so every hold and dark run is a
/// whole number of pairs.
const PAIR: usize = 2;
/// Shortest run a DMA descriptor streams: as long as a shift of a 64-column
/// panel. The DMA fetches each descriptor while the I2S drains its FIFO, so a
/// series of short runs empties the FIFO, and the ESP32's I2S stops for good
/// once it runs dry while the DMA keeps cycling.
const MIN_RUN: usize = 64;

/// Exponent of the on-time unit at full brightness: plane `k` is lit for
/// `2^(k + MAX_UNIT)` clocks. Longer units barely brighten the panel, since
/// the shifts' blank clocks are already a small share of each row.
const MAX_UNIT: i32 = 2;

/// Clocks plane `plane` is lit for at the given on-time unit exponent.
const fn lit_clocks(plane: usize, unit: i32) -> usize {
    let exponent = plane as i32 + unit;
    if exponent < 0 {
        0
    } else {
        1 << exponent
    }
}

/// Lit words in each of `reps` hold repetitions for a plane lit for `lit`
/// clocks, given `window` lit clocks fit in the following shift.
const fn hold_per_rep(lit: usize, window: usize, reps: usize) -> usize {
    if lit <= window {
        0
    } else {
        PAIR * (lit - window).div_ceil(PAIR * reps)
    }
}

/// Lit clocks one shift slot can carry.
const fn window_clocks<const COLS: usize>() -> usize {
    COLS - LEAD_BLANK_DELAY - 1 - TRAIL_BLANK_DELAY
}

/// Hold repetitions per plane, enough for every plane at full brightness.
/// Planes that fit in their window even then get none, which saves each row
/// a blank [`MIN_RUN`] per plane.
const fn hold_reps<const COLS: usize, const PLANES: usize>() -> [usize; PLANES] {
    let mut reps = [0; PLANES];
    let mut plane = 0;
    while plane < PLANES {
        let lit = lit_clocks(plane, MAX_UNIT);
        if lit > window_clocks::<COLS>() {
            reps[plane] = 1;
            while hold_per_rep(lit, window_clocks::<COLS>(), reps[plane]) > HOLD_WORDS {
                reps[plane] += 1;
            }
        }
        plane += 1;
    }
    reps
}

/// One DMA run in a row's sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Segment {
    Shift(usize),
    Hold(usize),
    Dark,
}

/// A row's segments in streaming order, and how many there are.
const fn layout<const COLS: usize, const PLANES: usize>(
) -> ([Segment; BCM_SEQUENCE_CAPACITY], usize) {
    let reps = hold_reps::<COLS, PLANES>();
    let mut segments = [Segment::Dark; BCM_SEQUENCE_CAPACITY];
    let mut len = 0;
    let mut plane = 0;
    while plane < PLANES {
        segments[len] = Segment::Shift(plane);
        len += 1;
        if reps[plane] > 0 {
            segments[len] = Segment::Hold(plane);
            len += 1;
        }
        plane += 1;
    }
    // The dark run closes the row
    (segments, len + 1)
}

/// Hold and dark words for every row, shared by all framebuffers.
#[repr(C, align(4))]
pub struct Holds<const NROWS: usize> {
    rows: [HoldRow; NROWS],
    dark: [Entry; DARK_WORDS],
}

#[derive(Clone, Copy)]
#[repr(C, align(4))]
struct HoldRow {
    lit: [Entry; HOLD_WORDS],
    /// Pads holds shorter than [`MIN_RUN`] after their lit words, so a hold
    /// is the last `hold` lit words followed by blank ones.
    blank: [Entry; MIN_RUN],
}

impl<const NROWS: usize> Holds<NROWS> {
    /// All-zero holds; call [`Self::format`] before use. An all-zero
    /// `static` lands in zero-initialized RAM, which the DMA can read.
    #[must_use]
    pub const fn unformatted() -> Self {
        Self {
            rows: [HoldRow {
                lit: [Entry::from_raw(0); HOLD_WORDS],
                blank: [Entry::from_raw(0); MIN_RUN],
            }; NROWS],
            dark: [Entry::from_raw(0); DARK_WORDS],
        }
    }

    /// Writes every row's address and output-enable bits.
    pub fn format(&mut self) {
        for (slot, row) in self.rows.iter_mut().enumerate() {
            let (_, addr) = slot_addresses::<NROWS>(slot);
            row.lit = [Entry::from_raw(u16::from(addr) | OE_ACTIVE); HOLD_WORDS];
            row.blank = [Entry::from_raw(u16::from(addr) | OE_BLANK); MIN_RUN];
        }
        // The address doesn't matter with every column driver off
        self.dark = [Entry::from_raw(OE_BLANK); DARK_WORDS];
    }
}

/// How long each plane is lit, as laid out in the shift windows, hold
/// repetitions and dark padding of every row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Timing<const PLANES: usize> {
    /// Lit clocks of each plane in the shift slot that follows it.
    window: [usize; PLANES],
    /// Lit words in each hold repetition of each plane; holds shorter than
    /// [`MIN_RUN`] are padded with blank words.
    hold: [usize; PLANES],
    /// Words in each dark repetition.
    dark: usize,
}

impl<const PLANES: usize> Timing<PLANES> {
    const OFF: Self = Self {
        window: [0; PLANES],
        hold: [0; PLANES],
        dark: MIN_RUN,
    };

    fn new<const COLS: usize>(unit: i32, dark: usize) -> Self {
        let reps = hold_reps::<COLS, PLANES>();
        let mut timing = Self { dark, ..Self::OFF };
        for plane in 0..PLANES {
            let lit = lit_clocks(plane, unit);
            let hold = match reps[plane] {
                0 => 0,
                reps => hold_per_rep(lit, window_clocks::<COLS>(), reps),
            };
            timing.hold[plane] = hold;
            timing.window[plane] = lit - hold * reps[plane];
        }
        timing
    }

    /// Clocks per row other than the dark padding.
    fn busy_clocks<const COLS: usize>(&self) -> usize {
        let reps = hold_reps::<COLS, PLANES>();
        let holds: usize = (0..PLANES)
            .map(|plane| reps[plane] * self.hold[plane].max(MIN_RUN))
            .sum();
        PLANES * COLS + holds
    }

    fn row_clocks<const COLS: usize>(&self) -> usize {
        self.busy_clocks::<COLS>() + DARK_REPS * self.dark
    }

    /// Picks the longest on-time unit that still dims the panel to
    /// `brightness` (of 255) without rows getting longer than `max_row_clocks`.
    fn for_brightness<const COLS: usize>(brightness: u8, max_row_clocks: usize) -> Self {
        if brightness == 0 {
            return Self::OFF;
        }
        let full = Self::new::<COLS>(MAX_UNIT, MIN_RUN);
        let full_row = full.row_clocks::<COLS>();
        assert!(
            full_row <= max_row_clocks,
            "rows take {full_row} clocks at full brightness, over the {max_row_clocks} allowed"
        );
        let full_lit = lit_total(MAX_UNIT, PLANES);
        let min_unit = 1 - PLANES as i32;
        for unit in (min_unit..=MAX_UNIT).rev() {
            let busy = Self::new::<COLS>(unit, 0).busy_clocks::<COLS>();
            // Row length that lights this unit's planes for the wanted share
            // of the row, relative to full brightness
            let row =
                lit_total(unit, PLANES) * full_row * 255 / (full_lit * usize::from(brightness));
            let dark = (row.saturating_sub(busy) / DARK_REPS / PAIR * PAIR).max(MIN_RUN);
            if dark <= DARK_WORDS && busy + DARK_REPS * dark <= max_row_clocks {
                return Self::new::<COLS>(unit, dark);
            }
        }
        unreachable!("a single lit clock per row fits any row the full brightness fits")
    }
}

fn lit_total(unit: i32, planes: usize) -> usize {
    (0..planes).map(|plane| lit_clocks(plane, unit)).sum()
}

/// Row-major framebuffer whose planes are shifted once per row and held lit;
/// see the [module docs](self).
#[repr(C, align(4))]
pub struct DmaFrameBuffer<const NROWS: usize, const COLS: usize, const PLANES: usize> {
    rows: [[[Entry; COLS]; PLANES]; NROWS],
    /// `None` until formatted.
    holds: Option<&'static Holds<NROWS>>,
    timing: Timing<PLANES>,
}

impl<const NROWS: usize, const COLS: usize, const PLANES: usize>
    DmaFrameBuffer<NROWS, COLS, PLANES>
{
    const LAYOUT: [Segment; BCM_SEQUENCE_CAPACITY] = layout::<COLS, PLANES>().0;
    const SEQUENCE_LEN: usize = layout::<COLS, PLANES>().1;

    /// Pixel clocks a row takes at full brightness, the shortest it gets.
    pub const FULL_BRIGHTNESS_ROW_CLOCKS: usize = {
        let reps = hold_reps::<COLS, PLANES>();
        let mut clocks = PLANES * COLS + DARK_REPS * MIN_RUN;
        let mut plane = 0;
        while plane < PLANES {
            if reps[plane] > 0 {
                let hold = hold_per_rep(
                    lit_clocks(plane, MAX_UNIT),
                    window_clocks::<COLS>(),
                    reps[plane],
                );
                clocks += reps[plane] * if hold > MIN_RUN { hold } else { MIN_RUN };
            }
            plane += 1;
        }
        clocks
    };

    /// Creates an all-zero frame buffer; call [`Self::format`] before use.
    ///
    /// An all-zero `static` is placed in zero-initialized RAM instead of
    /// being stored in the firmware image.
    ///
    /// # Panics
    /// Panics if `NROWS` is not within `1..=32` (5-bit row address), `PLANES`
    /// is not within `1..=8`, or `COLS` is odd or too short to hold the
    /// blanking. In const contexts this is a compile-time error.
    #[must_use]
    pub const fn unformatted() -> Self {
        assert!(NROWS >= 1 && NROWS <= 32, "NROWS must be within 1..=32");
        assert!(PLANES >= 1 && PLANES <= 8, "PLANES must be within 1..=8");
        assert!(COLS % 2 == 0, "rows must be whole 32-bit DMA words");
        assert!(
            COLS >= MIN_RUN,
            "a shift is a DMA run, so it can't be shorter than MIN_RUN"
        );
        assert!(
            COLS > LEAD_BLANK_DELAY + 1 + TRAIL_BLANK_DELAY,
            "COLS must leave room for an output-enable window"
        );
        assert!(
            COLS * 2 <= DARK_WORDS * 2,
            "a row must fit in one DMA descriptor"
        );
        Self {
            rows: [[[Entry::from_raw(0); COLS]; PLANES]; NROWS],
            holds: None,
            timing: Timing {
                window: [0; PLANES],
                hold: [0; PLANES],
                dark: 0,
            },
        }
    }

    /// Writes the row addresses and control bits, clears every pixel and
    /// turns the panel off until [`Self::set_brightness`].
    pub fn format(&mut self, holds: &'static Holds<NROWS>) {
        for (slot, row) in self.rows.iter_mut().enumerate() {
            let (prev_addr, addr) = slot_addresses::<NROWS>(slot);
            for (plane, data) in row.iter_mut().enumerate() {
                // The first shift keeps lighting the previous row
                let addr = if plane == 0 { prev_addr } else { addr };
                *data = [Entry::from_raw(u16::from(addr) | OE_BLANK); COLS];
                data[map_index(COLS - 1)].0 |= LATCH;
            }
        }
        self.holds = Some(holds);
        self.timing = Timing::OFF;
        self.write_windows();
    }

    /// Dims the panel to `brightness` (of 255) while keeping rows within
    /// `max_row_clocks` pixel clocks, i.e. the refresh rate above
    /// `pixel clock / (max_row_clocks × NROWS)`.
    ///
    /// # Panics
    /// Panics if `max_row_clocks` is below
    /// [`Self::FULL_BRIGHTNESS_ROW_CLOCKS`].
    pub fn set_brightness(&mut self, brightness: u8, max_row_clocks: usize) {
        let timing = Timing::for_brightness::<COLS>(brightness, max_row_clocks);
        let windows_changed = timing.window != self.timing.window;
        self.timing = timing;
        if windows_changed {
            self.write_windows();
        }
    }

    /// Pixel clocks each row currently takes.
    #[must_use]
    pub fn row_clocks(&self) -> usize {
        self.timing.row_clocks::<COLS>()
    }

    fn write_windows(&mut self) {
        let window = self.timing.window;
        for row in &mut self.rows {
            for (plane, data) in row.iter_mut().enumerate() {
                let shown = if plane == 0 { PLANES - 1 } else { plane - 1 };
                for i in TRAIL_BLANK_DELAY..TRAIL_BLANK_DELAY + window_clocks::<COLS>() {
                    let oe = if i < TRAIL_BLANK_DELAY + window[shown] {
                        OE_ACTIVE
                    } else {
                        OE_BLANK
                    };
                    let entry = &mut data[map_index(i)];
                    entry.0 = (entry.0 & !OE_MASK) | oe;
                }
            }
        }
    }

    /// Set a pixel in the framebuffer.
    #[inline]
    pub fn set_pixel(&mut self, p: Point, color: Color) {
        if p.x < 0 || p.y < 0 {
            return;
        }
        let (x, y) = (p.x as usize, p.y as usize);
        if x >= COLS || y >= NROWS * 2 {
            return;
        }
        let is_top = y < NROWS;
        let slot = map_row_index::<NROWS>(if is_top { y } else { y - NROWS });
        let (red, green, blue) = (color.r(), color.g(), color.b());
        let col = map_index(x);
        for (plane, data) in self.rows[slot].iter_mut().enumerate() {
            let bit = (8 - PLANES + plane) as u32;
            let bits =
                (((blue >> bit) & 1) << 2) | (((green >> bit) & 1) << 1) | ((red >> bit) & 1);
            if is_top {
                data[col].set_color0_bits(bits);
            } else {
                data[col].set_color1_bits(bits);
            }
        }
    }

    fn holds(&self) -> &'static Holds<NROWS> {
        self.holds
            .expect("the framebuffer is formatted before it is shown")
    }
}

impl<const NROWS: usize, const COLS: usize, const PLANES: usize> core::fmt::Debug
    for DmaFrameBuffer<NROWS, COLS, PLANES>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaFrameBuffer")
            .field("size", &core::mem::size_of_val(&self.rows))
            .field("timing", &self.timing)
            .finish()
    }
}

const fn bcm_sequence<const COLS: usize, const PLANES: usize>(
) -> [BcmLenReps; BCM_SEQUENCE_CAPACITY] {
    let reps = hold_reps::<COLS, PLANES>();
    let (segments, len) = layout::<COLS, PLANES>();
    let word = core::mem::size_of::<Entry>();
    let mut seq = [BcmLenReps::ZERO; BCM_SEQUENCE_CAPACITY];
    let mut i = 0;
    while i < len {
        // Holds and dark runs at the longest they get; descriptors are sized
        // from these
        seq[i] = match segments[i] {
            Segment::Shift(_) => BcmLenReps {
                len: COLS * word,
                reps: 1,
            },
            Segment::Hold(plane) => BcmLenReps {
                len: HOLD_WORDS * word,
                reps: reps[plane],
            },
            Segment::Dark => BcmLenReps {
                len: DARK_WORDS * word,
                reps: DARK_REPS,
            },
        };
        i += 1;
    }
    seq
}

impl<const NROWS: usize, const COLS: usize, const PLANES: usize> FrameBuffer
    for DmaFrameBuffer<NROWS, COLS, PLANES>
{
    type Word = u16;

    /// Holds the longest each segment gets; the actual lengths follow the
    /// brightness, see [`FrameBuffer::bcm_segment`].
    const BCM_SEQUENCE: [BcmLenReps; BCM_SEQUENCE_CAPACITY] = bcm_sequence::<COLS, PLANES>();

    const BCM_SEQUENCE_LEN: usize = Self::SEQUENCE_LEN;

    const BCM_SEQUENCE_COUNT: usize = NROWS;

    fn bcm_segment_ptr(&self, index: usize) -> *const u8 {
        self.bcm_segment(index).ptr
    }

    fn bcm_segment(&self, index: usize) -> BcmSegment {
        assert!(
            index < Self::BCM_SEGMENT_COUNT,
            "segment index {index} out of range for {} segments",
            Self::BCM_SEGMENT_COUNT
        );
        let slot = index / Self::SEQUENCE_LEN;
        let position = index % Self::SEQUENCE_LEN;
        let word = core::mem::size_of::<Entry>();
        let reps = Self::BCM_SEQUENCE[position].reps;
        let hold_row = &self.holds().rows[slot];
        let (ptr, words): (*const Entry, usize) = match Self::LAYOUT[position] {
            Segment::Shift(plane) => (self.rows[slot][plane].as_ptr(), COLS),
            Segment::Hold(plane) => {
                let hold = self.timing.hold[plane];
                // Taken from the whole row, since a short hold runs on from
                // `lit` into `blank`, which follows it (`repr(C)`)
                let start = core::ptr::from_ref(hold_row)
                    .cast::<Entry>()
                    .wrapping_add(HOLD_WORDS - hold);
                (start, hold.max(MIN_RUN))
            }
            Segment::Dark => (self.holds().dark.as_ptr(), self.timing.dark),
        };
        BcmSegment {
            ptr: ptr.cast(),
            len: words * word,
            reps,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::boxed::Box;
    use std::vec::Vec;

    use super::*;

    const COLS: usize = 64;
    const NROWS: usize = 32;
    const PLANES: usize = 8;
    type TestBuffer = DmaFrameBuffer<NROWS, COLS, PLANES>;
    /// 180 Hz at 19 MHz.
    const MAX_ROW: usize = 19_000_000 / (180 * NROWS);

    fn formatted() -> Box<TestBuffer> {
        let holds = Box::leak(Box::new(Holds::<NROWS>::unformatted()));
        holds.format();
        let mut fb = Box::new(TestBuffer::unformatted());
        fb.format(holds);
        fb
    }

    /// Clocks each plane is lit per row, read back from the segments the DMA
    /// streams, and the clocks the row takes.
    fn lit_per_plane(fb: &TestBuffer, slot: usize) -> ([usize; PLANES], usize) {
        let mut lit = [0; PLANES];
        let mut clocks = 0;
        let mut latched: Option<usize> = None;
        for position in 0..TestBuffer::SEQUENCE_LEN {
            let segment = fb.bcm_segment(slot * TestBuffer::SEQUENCE_LEN + position);
            let words = unsafe {
                core::slice::from_raw_parts(segment.ptr.cast::<Entry>(), segment.len / 2)
            };
            for _ in 0..segment.reps {
                for i in 0..words.len() {
                    let entry = words[map_index(i)];
                    clocks += 1;
                    if entry.0 & OE_MASK == OE_ACTIVE {
                        let plane = latched.unwrap_or(PLANES - 1);
                        lit[plane] += 1;
                    }
                    if let (true, Segment::Shift(plane)) =
                        (entry.latch(), TestBuffer::LAYOUT[position])
                    {
                        latched = Some(plane);
                    }
                }
            }
        }
        (lit, clocks)
    }

    #[test]
    fn every_unit_lights_planes_by_their_weight() {
        for unit in (1 - PLANES as i32)..=MAX_UNIT {
            let timing = Timing::<PLANES>::new::<COLS>(unit, MIN_RUN);
            let reps = hold_reps::<COLS, PLANES>();
            for plane in 0..PLANES {
                assert!(timing.window[plane] <= window_clocks::<COLS>());
                assert!(timing.hold[plane] <= HOLD_WORDS);
                assert_eq!(timing.hold[plane] % PAIR, 0);
                assert_eq!(
                    timing.window[plane] + timing.hold[plane] * reps[plane],
                    lit_clocks(plane, unit),
                    "unit {unit}, plane {plane}"
                );
            }
        }
    }

    #[test]
    fn streamed_segments_light_planes_by_their_weight() {
        let mut fb = formatted();
        for brightness in [255, 51, 10, 1] {
            fb.set_brightness(brightness, MAX_ROW);
            let (lit, clocks) = lit_per_plane(&fb, 5);
            assert_eq!(clocks, fb.row_clocks());
            // The first shift lights the previous row's MSB, and every row
            // is laid out alike
            let unit = lit[PLANES - 1].ilog2() as i32 - (PLANES as i32 - 1);
            for plane in 0..PLANES {
                assert_eq!(
                    lit[plane],
                    lit_clocks(plane, unit),
                    "brightness {brightness}, plane {plane}: {lit:?}"
                );
            }
        }
    }

    #[test]
    fn full_brightness_row_matches_its_constant() {
        let mut fb = formatted();
        fb.set_brightness(255, MAX_ROW);
        assert_eq!(fb.row_clocks(), TestBuffer::FULL_BRIGHTNESS_ROW_CLOCKS);
    }

    #[test]
    fn brightness_scales_the_lit_share_of_each_row() {
        let full = Timing::<PLANES>::for_brightness::<COLS>(255, MAX_ROW);
        let share = |t: &Timing<PLANES>| {
            let lit: usize = (0..PLANES)
                .map(|p| t.window[p] + t.hold[p] * hold_reps::<COLS, PLANES>()[p])
                .sum();
            lit as f64 / t.row_clocks::<COLS>() as f64
        };
        let full_share = share(&full);
        let mut previous = 0.0;
        for brightness in 1..=255u8 {
            let timing = Timing::<PLANES>::for_brightness::<COLS>(brightness, MAX_ROW);
            assert!(timing.row_clocks::<COLS>() <= MAX_ROW);
            let relative = share(&timing) / full_share;
            let wanted = f64::from(brightness) / 255.0;
            assert!(
                (relative - wanted).abs() <= wanted * 0.02 + 0.001,
                "brightness {brightness}: {relative} instead of {wanted}"
            );
            assert!(relative >= previous, "brightness {brightness} got dimmer");
            previous = relative;
        }
    }

    #[test]
    fn dim_brightness_keeps_every_plane_down_to_one_clock() {
        let timing = Timing::<PLANES>::for_brightness::<COLS>(51, MAX_ROW);
        assert!(timing.window[0] >= 1, "{timing:?}");
    }

    #[test]
    fn off_lights_nothing() {
        let mut fb = formatted();
        fb.set_brightness(0, MAX_ROW);
        let (lit, _) = lit_per_plane(&fb, 0);
        assert_eq!(lit, [0; PLANES]);
    }

    #[test]
    fn segments_fit_their_static_maximum() {
        let mut fb = formatted();
        for brightness in [255, 128, 51, 5, 1, 0] {
            fb.set_brightness(brightness, MAX_ROW);
            for index in 0..TestBuffer::BCM_SEGMENT_COUNT {
                let segment = fb.bcm_segment(index);
                let max = TestBuffer::BCM_SEQUENCE[index % TestBuffer::SEQUENCE_LEN];
                assert!(segment.len >= MIN_RUN * 2 && segment.len <= max.len);
                assert_eq!(segment.len % 4, 0);
                assert_eq!(segment.ptr as usize % 4, 0);
            }
        }
    }

    #[test]
    fn address_changes_once_per_row_with_the_panel_blank() {
        let mut fb = formatted();
        fb.set_brightness(255, MAX_ROW);
        let mut words = Vec::new();
        for index in 0..TestBuffer::BCM_SEGMENT_COUNT {
            let segment = fb.bcm_segment(index);
            let slice = unsafe {
                core::slice::from_raw_parts(segment.ptr.cast::<Entry>(), segment.len / 2)
            };
            for _ in 0..segment.reps {
                words.extend((0..slice.len()).map(|i| slice[map_index(i)]));
            }
        }
        let lit: Vec<&Entry> = words
            .iter()
            .filter(|e| e.0 & OE_MASK == OE_ACTIVE)
            .collect();
        let mut changes = 0;
        for pair in lit.windows(2) {
            if pair[0].addr() != pair[1].addr() {
                changes += 1;
            }
        }
        // The first shift still lights the last row of the previous pass
        assert_eq!(changes, NROWS);
        for (i, pair) in words.windows(2).enumerate() {
            if pair[0].addr() != pair[1].addr() {
                for entry in &words[i + 1..i + 1 + TRAIL_BLANK_DELAY] {
                    assert_eq!(
                        entry.0 & OE_MASK,
                        OE_BLANK,
                        "lit right after an address change"
                    );
                }
            }
        }
    }

    #[test]
    fn set_pixel_writes_each_bit_into_its_plane() {
        let mut fb = formatted();
        let color = Color::new(0b1010_0101, 0b0101_1010, 0b1111_0000);
        fb.set_pixel(Point::new(2, 3), color);
        fb.set_pixel(Point::new(4, 3 + NROWS as i32), color);
        for plane in 0..PLANES {
            let top = fb.rows[map_row_index::<NROWS>(3)][plane][map_index(2)];
            let bottom = fb.rows[map_row_index::<NROWS>(3)][plane][map_index(4)];
            assert_eq!(top.red1(), (color.r() >> plane) & 1 != 0);
            assert_eq!(top.grn1(), (color.g() >> plane) & 1 != 0);
            assert_eq!(top.blu1(), (color.b() >> plane) & 1 != 0);
            assert_eq!(bottom.red2(), (color.r() >> plane) & 1 != 0);
            assert_eq!(bottom.blu2(), (color.b() >> plane) & 1 != 0);
        }
    }

    #[test]
    fn brightness_keeps_pixels() {
        let mut fb = formatted();
        fb.set_pixel(Point::new(7, 1), Color::WHITE);
        fb.set_brightness(51, MAX_ROW);
        fb.set_brightness(255, MAX_ROW);
        assert!(fb.rows[map_row_index::<NROWS>(1)][0][map_index(7)].red1());
    }
}
