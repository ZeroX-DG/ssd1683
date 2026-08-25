#[cfg(not(feature = "use_red"))]
use embedded_graphics_core::pixelcolor::raw::RawU1;
#[cfg(feature = "use_red")]
use embedded_graphics_core::pixelcolor::raw::RawU2;
use embedded_graphics_core::pixelcolor::{Rgb555, Rgb565, Rgb888};
use embedded_graphics_core::prelude::{PixelColor, RgbColor};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EpdColor {
    Black,
    White,
    #[cfg(feature = "use_red")]
    Red,
}

impl EpdColor {
    pub fn black_bit(&self) -> u8 {
        match self {
            EpdColor::Black => 0b0,
            EpdColor::White => 0b1,
            #[cfg(feature = "use_red")]
            EpdColor::Red => 0b1,
        }
    }

    #[cfg(feature = "use_red")]
    pub fn red_bit(&self) -> u8 {
        match self {
            EpdColor::Black | EpdColor::White => 0b0,
            EpdColor::Red => 0b1,
        }
    }
}

#[cfg(feature = "use_red")]
impl PixelColor for EpdColor {
    type Raw = RawU2;
}

#[cfg(not(feature = "use_red"))]
impl PixelColor for EpdColor {
    type Raw = RawU1;
}

impl From<u8> for EpdColor {
    fn from(value: u8) -> Self {
        match value {
            0b00 => EpdColor::Black,
            0b01 => EpdColor::White,
            #[cfg(feature = "use_red")]
            0b10 | 0b11 => EpdColor::Red,
            _ => EpdColor::White,
        }
    }
}

fn from_rgb<C: RgbColor>(color: C) -> EpdColor {
    let r = color.r() as u32;
    let g = color.g() as u32;
    let b = color.b() as u32;
    let max_r = C::MAX_R as u32;
    let max_g = C::MAX_G as u32;

    #[cfg(feature = "use_red")]
    {
        let max_b = C::MAX_B as u32;
        // Red-dominant: r is clearly the largest channel relative to its max.
        if r * max_g > g * max_r && r * max_b > b * max_r {
            return EpdColor::Red;
        }
    }

    // Luma (ITU-R BT.601) using integer weights, normalized to 8-bit.
    let luma = (r * max_g * 299 + g * max_r * 587 + b * max_r * 114) / (max_r * max_g * 1000 / 255);

    if luma < 128 {
        EpdColor::Black
    } else {
        EpdColor::White
    }
}

impl From<Rgb555> for EpdColor {
    fn from(color: Rgb555) -> Self {
        from_rgb(color)
    }
}

impl From<Rgb565> for EpdColor {
    fn from(color: Rgb565) -> Self {
        from_rgb(color)
    }
}

impl From<Rgb888> for EpdColor {
    fn from(color: Rgb888) -> Self {
        from_rgb(color)
    }
}
