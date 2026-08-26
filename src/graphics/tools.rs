use crate::graphics::config::Rotation;

#[derive(Debug)]
pub struct DirtyRect {
    pub min_byte_col: u8,
    pub max_byte_col: u8,
    pub min_y: u16,
    pub max_y: u16,
}

pub struct RegionIterator<'buf> {
    buffer: &'buf [u8],
    stride: usize,
    current_y: usize,
    end_y: usize,
    col_start: usize,
    col_len: usize,
}

pub fn rotation(x: u32, y: u32, width: u32, height: u32, rotation: Rotation) -> (u32, u8) {
    match rotation {
        Rotation::Rotate0 => (x / 8 + (width / 8) * y, 0x80 >> (x % 8)),
        Rotation::Rotate90 => ((width - 1 - y) / 8 + (width / 8) * x, 0x01 << (y % 8)),
        Rotation::Rotate180 => (
            ((width / 8) * height - 1) - (x / 8 + (width / 8) * y),
            0x01 << (x % 8),
        ),
        Rotation::Rotate270 => (y / 8 + (height - 1 - x) * (width / 8), 0x80 >> (y % 8)),
    }
}

pub fn calculate_dirty_area(buffer: &[u8], width: u32) -> Option<DirtyRect> {
    if width == 0 || buffer.is_empty() {
        return None;
    }

    let stride = (width / 8) as usize;

    let mut any_change = false;
    let mut min_byte_col = u8::MAX;
    let mut max_byte_col = 0;
    let mut min_y = u16::MAX;
    let mut max_y = 0;

    for (y, row) in buffer.chunks(stride).enumerate() {
        let mut row_has_change = false;

        for (byte_col, &byte) in row.iter().enumerate() {
            if byte == 0 {
                continue;
            }
            row_has_change = true;
            let byte_col = byte_col as u8;

            if byte_col < min_byte_col {
                min_byte_col = byte_col;
            }
            if byte_col > max_byte_col {
                max_byte_col = byte_col;
            }
        }

        if row_has_change {
            any_change = true;
            let y = y as u16;
            if y < min_y {
                min_y = y;
            }
            if y > max_y {
                max_y = y;
            }
        }
    }

    // `0` is a valid coordinate for both `max_byte_col` and `max_y`, so it can't
    // double as a "nothing changed" sentinel: a change confined to the top-left
    // corner (byte column 0, row 0) would be reported as a clean buffer and the
    // frame would never be pushed to the panel. Track emptiness explicitly.
    if !any_change {
        None
    } else {
        Some(DirtyRect {
            min_byte_col,
            max_byte_col,
            min_y,
            max_y,
        })
    }
}

impl<'buf> RegionIterator<'buf> {
    pub fn new(buffer: &'buf [u8], width_px: usize, rect: &DirtyRect) -> Self {
        Self {
            buffer,
            stride: width_px / 8,
            current_y: rect.min_y as usize,
            end_y: rect.max_y as usize,
            col_start: rect.min_byte_col as usize,
            col_len: (rect.max_byte_col - rect.min_byte_col + 1) as usize,
        }
    }
}

impl<'buf> Iterator for RegionIterator<'buf> {
    type Item = &'buf [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_y > self.end_y {
            return None;
        }

        let start = self.current_y * self.stride + self.col_start;
        let end = start + self.col_len;

        self.current_y += 1;

        Some(&self.buffer[start..end])
    }
}
