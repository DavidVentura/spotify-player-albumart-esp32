use album_art_core::protocol::SIZE;

use crate::color::Rgb;

/// A full panel image in sRGB, indexed as `pixels[y][x]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub pixels: Box<[[Rgb; SIZE]; SIZE]>,
}

impl Frame {
    pub fn black() -> Self {
        let rows = vec![[Rgb::default(); SIZE]; SIZE];
        let pixels = rows
            .into_boxed_slice()
            .try_into()
            .expect("SIZE rows were allocated");
        Frame { pixels }
    }
}
