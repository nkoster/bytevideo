//! Dense radix blocks and frame-local BV02/BVA1 transport. The legacy layout
//! remains the wire format for two/four levels. Other alphabets use this layout.
use crate::{gray_level, repair, tile_chroma, ColorMode, FRAME_BYTES, HEIGHT, PACKET_BYTES, WIDTH};
use std::io;

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub bytes: usize,
    pub digits: usize,
    pub capacity: usize,
    size: usize,
    levels: usize,
    cols: usize,
}

impl Layout {
    pub fn enabled(levels: usize) -> bool {
        levels != 2 && levels != 4
    }

    pub fn new(size: usize, levels: usize) -> Self {
        // Up to 56 data bits keep all radix arithmetic within u64, including
        // invalid codewords. Prefer smaller blocks when efficiency is equal:
        // a damaged tile then affects fewer bytes.
        let mut bytes = 1;
        let mut digits = 8;
        for n in 1..=7 {
            let mut count = 1;
            let mut values = levels as u64;
            while values < (1u64 << (n * 8)) {
                values *= levels as u64;
                count += 1;
            }
            if count * bytes < digits * n {
                bytes = n;
                digits = count;
            }
        }
        let cols = WIDTH / size;
        let blocks = (cols * (HEIGHT / size)) / digits;
        Self {
            bytes,
            digits,
            capacity: blocks * bytes,
            size,
            levels,
            cols,
        }
    }

    pub fn frames(self, packets: u64) -> u64 {
        let mut frames = 0;
        let mut used = 0;
        for _ in 0..packets {
            if self.capacity - used < repair::BYTES {
                frames += 1;
                used = 0;
            }
            if self.capacity - used >= PACKET_BYTES {
                used += PACKET_BYTES;
            } else {
                for _ in 0..repair::FULL_PARTS {
                    if used + repair::BYTES > self.capacity {
                        frames += 1;
                        used = 0;
                    }
                    used += repair::BYTES;
                }
            }
            if used == self.capacity {
                frames += 1;
                used = 0;
            }
        }
        frames + u64::from(used > 0)
    }

    pub fn decode(self, frame: &[u8]) -> Vec<u8> {
        let margin = self.size / 4;
        let samples = (self.size - 2 * margin).pow(2);
        let thresholds: Vec<_> = (0..self.levels - 1)
            .map(|i| {
                samples
                    * (usize::from(gray_level(i, self.levels))
                        + usize::from(gray_level(i + 1, self.levels)))
            })
            .collect();
        let mut out = vec![0; self.capacity];
        for (block, bytes) in out.chunks_mut(self.bytes).enumerate() {
            let mut value = 0u64;
            for digit in 0..self.digits {
                let tile = block * self.digits + digit;
                let x = (tile % self.cols) * self.size;
                let y = (tile / self.cols) * self.size;
                let mut sum = 0;
                for dy in margin..self.size - margin {
                    for dx in margin..self.size - margin {
                        sum += frame[(y + dy) * WIDTH + x + dx] as usize;
                    }
                }
                let symbol = thresholds.iter().filter(|&&t| sum * 2 >= t).count();
                value = value * self.levels as u64 + symbol as u64;
            }
            if value < (1u64 << (8 * self.bytes)) {
                bytes.copy_from_slice(&value.to_be_bytes()[8 - self.bytes..]);
            }
            // Unused codewords become zero bytes; packet/fragment CRCs reject
            // damaged transport units independently of neighboring units.
        }
        out
    }
}

pub struct Renderer {
    layout: Layout,
    color: bool,
    mode: ColorMode,
    frame: Vec<u8>,
}

impl Renderer {
    #[cfg(test)]
    pub fn new(layout: Layout, color: bool) -> Self {
        Self::with_mode(
            layout,
            if color {
                ColorMode::Rasta
            } else {
                ColorMode::Gray
            },
        )
    }

    pub fn with_mode(layout: Layout, mode: ColorMode) -> Self {
        let color = mode != ColorMode::Gray;
        let mut frame = vec![
            128;
            if color {
                FRAME_BYTES * 3 / 2
            } else {
                FRAME_BYTES
            }
        ];
        frame[..FRAME_BYTES].fill(if color { 30 } else { 16 });
        Self {
            layout,
            color,
            mode,
            frame,
        }
    }

    pub fn render(&mut self, data: &[u8]) -> &[u8] {
        let layout = self.layout;
        assert_eq!(data.len(), layout.capacity);
        let mut digits = vec![0; layout.digits];
        for (block, bytes) in data.chunks(layout.bytes).enumerate() {
            let mut value = bytes.iter().fold(0u64, |n, &b| (n << 8) | u64::from(b));
            for digit in digits.iter_mut().rev() {
                *digit = (value % layout.levels as u64) as usize;
                value /= layout.levels as u64;
            }
            for (digit, &symbol) in digits.iter().enumerate() {
                let tile = block * layout.digits + digit;
                let col = tile % layout.cols;
                let row = tile / layout.cols;
                let x = col * layout.size;
                let y = row * layout.size;
                let level = gray_level(symbol, layout.levels);
                let luma = if self.color {
                    (16 + (u16::from(level) * 219 + 127) / 255) as u8
                } else {
                    level
                };
                for dy in 0..layout.size {
                    let start = (y + dy) * WIDTH + x;
                    self.frame[start..start + layout.size].fill(luma);
                }
                if self.color {
                    let (u, v) = tile_chroma(level, self.mode.hue(col + row, layout.levels));
                    for cy in y.div_ceil(2)..(y + layout.size).div_ceil(2) {
                        let start = FRAME_BYTES + cy * (WIDTH / 2) + x.div_ceil(2);
                        let end = start + (x + layout.size).div_ceil(2) - x.div_ceil(2);
                        self.frame[start..end].fill(u);
                        self.frame[start + FRAME_BYTES / 4..end + FRAME_BYTES / 4].fill(v);
                    }
                }
            }
        }
        &self.frame
    }
}

pub struct Packer {
    capacity: usize,
    pending: Vec<u8>,
}

impl Packer {
    pub fn new(layout: Layout) -> Self {
        Self {
            capacity: layout.capacity,
            pending: Vec::with_capacity(layout.capacity),
        }
    }

    pub fn packet(
        &mut self,
        index: u32,
        packet: &[u8],
        mut emit: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        // If even a fragment cannot fit, start a new frame with a whole
        // packet rather than paying fragment overhead on the next frame.
        if self.capacity - self.pending.len() < repair::BYTES {
            self.flush(&mut emit)?;
        }
        if self.capacity - self.pending.len() >= PACKET_BYTES {
            self.pending.extend_from_slice(packet);
        } else {
            // Use the remaining space rather than reserving the bottom of each
            // frame. Every fragment is independently CRC-protected and may be
            // recovered after frame loss, duplication or reordering.
            for part in repair::full_fragments(index, packet) {
                if self.pending.len() + part.len() > self.capacity {
                    self.flush(&mut emit)?;
                }
                self.pending.extend_from_slice(&part);
            }
        }
        if self.pending.len() == self.capacity {
            self.flush(&mut emit)?;
        }
        Ok(())
    }

    pub fn finish(&mut self, emit: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.flush(emit)?;
        }
        Ok(())
    }

    fn flush(&mut self, mut emit: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        self.pending.resize(self.capacity, 0);
        emit(&self.pending)?;
        self.pending.clear();
        Ok(())
    }
}

/// Scan frame-local transport units. A damaged header does not hide subsequent
/// packets/fragments, and padding cannot establish a format without a valid CRC.
pub fn units(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        while offset + repair::BYTES <= bytes.len() {
            let remaining = &bytes[offset..];
            let len = if remaining.starts_with(b"BV02")
                && remaining.len() >= PACKET_BYTES
                && crate::parse_packet(&remaining[..PACKET_BYTES]).is_some()
            {
                PACKET_BYTES
            } else if repair::valid_fragment(&remaining[..repair::BYTES]) {
                repair::BYTES
            } else {
                offset += 1;
                continue;
            };
            offset += len;
            return Some(&remaining[..len]);
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{make_packet, make_shards, parse_packet, TileGeometry, SHARDS};
    use std::collections::BTreeMap;

    #[test]
    fn blocks_roundtrip_at_every_size_and_level_in_gray_and_color() {
        for levels in 3..=10 {
            if !Layout::enabled(levels) {
                continue;
            }
            for size in 2..=10 {
                let layout = Layout::new(size, levels);
                let data: Vec<_> = (0..layout.capacity)
                    .map(|i| match (i / layout.bytes) % 3 {
                        0 => 255,
                        1 => 0,
                        _ => (i * 71 + i / 256) as u8,
                    })
                    .collect();
                for color in [false, true] {
                    let mut renderer = Renderer::new(layout, color);
                    let frame = renderer.render(&data);
                    let gray: Vec<_> = frame[..FRAME_BYTES]
                        .iter()
                        .map(|&y| {
                            if color {
                                (((u16::from(y) - 16) * 255 + 109) / 219) as u8
                            } else {
                                y
                            }
                        })
                        .collect();
                    assert_eq!(
                        layout.decode(&gray),
                        data,
                        "size {size}, levels {levels}, color {color}"
                    );
                }
                // Even completely invalid radix blocks must not overflow.
                assert_eq!(
                    layout.decode(&vec![235; FRAME_BYTES]).len(),
                    layout.capacity
                );
            }
        }
        let ten = Layout::new(8, 10);
        assert_eq!((ten.bytes, ten.digits), (7, 17));
        // Previously 108 frames.
        assert_eq!(ten.frames(216), 69);
        // Nine levels leave less than one fragment after three whole packets:
        // start the next frame without fragmenting an otherwise whole packet.
        assert_eq!(Layout::new(8, 9).frames(216), 72);
    }

    #[test]
    fn frame_plans_match_transport_and_reassemble_reordered_units() {
        let data: Vec<_> = (0..60000).map(|i| (i * 71) as u8).collect();
        let (meta, shards) = make_shards(&data);
        for levels in 3..=10 {
            if !Layout::enabled(levels) {
                continue;
            }
            for size in 2..=10 {
                let layout = Layout::new(size, levels);
                let mut packer = Packer::new(layout);
                let mut frames = Vec::new();
                let mut original = BTreeMap::new();
                for index in 0..216u32 {
                    let packet_index = index % 72;
                    let packet = make_packet(
                        packet_index,
                        &meta,
                        &shards[packet_index as usize / SHARDS][packet_index as usize % SHARDS],
                    );
                    original.insert(packet_index, packet.clone());
                    packer
                        .packet(packet_index, &packet, |bytes| {
                            frames.push(bytes.to_vec());
                            Ok(())
                        })
                        .unwrap();
                }
                packer
                    .finish(|bytes| {
                        frames.push(bytes.to_vec());
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(frames.len() as u64, layout.frames(216));
                let old = TileGeometry::with_levels(size, levels);
                let old_frames = (216 * old.units_per_packet()).div_ceil(old.units_per_frame());
                assert!(frames.len() <= old_frames, "size {size}, levels {levels}");
                for frame in &frames[..frames.len() - 1] {
                    let filled: usize = units(frame).map(|unit| unit.len()).sum();
                    assert!(layout.capacity - filled < repair::BYTES);
                }
                let mut assembler = repair::Assembler::new().unwrap();
                let mut recovered = BTreeMap::new();
                // Reverse frame and unit order; repeated copies exercise duplicates.
                for frame in frames.iter().rev() {
                    for unit in units(frame).collect::<Vec<_>>().into_iter().rev() {
                        let packet = if unit.len() == PACKET_BYTES {
                            Some(unit.to_vec())
                        } else {
                            assembler.accept(unit).unwrap()
                        };
                        if let Some(packet) = packet {
                            let (index, _, _) = parse_packet(&packet).unwrap();
                            recovered.insert(index, packet);
                        }
                    }
                }
                assert_eq!(recovered, original, "size {size}, levels {levels}");
            }
        }
    }

    #[test]
    fn scanner_skips_corrupted_units_and_padding() {
        let (meta, shards) = make_shards(b"test");
        let packet = make_packet(0, &meta, &shards[0][0]);
        let mut damaged = packet.clone();
        damaged[0] = 0;
        let fragments = repair::full_fragments(0, &packet);
        let mut frame = damaged;
        frame.extend_from_slice(&packet);
        frame.extend_from_slice(&fragments[0]);
        frame.extend_from_slice(&[0; 300]);
        assert_eq!(
            units(&frame).collect::<Vec<_>>(),
            [&packet[..], &fragments[0][..]]
        );
    }
}
