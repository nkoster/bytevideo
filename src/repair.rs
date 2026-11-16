//! Small, CRC-protected audio fragments of ordinary BV02 packets.
use crate::streaming::Workspace;
use crate::{crc32, PACKET_BYTES};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub const BYTES: usize = 256;
const HEADER: usize = 18;
const PAYLOAD: usize = BYTES - HEADER;
pub const FULL_PARTS: usize = PACKET_BYTES.div_ceil(PAYLOAD);

pub fn full_fragments(index: u32, packet: &[u8]) -> Vec<Vec<u8>> {
    split(index, packet, packet.len())
}

pub fn valid_fragment(fragment: &[u8]) -> bool {
    parse_fragment(fragment).is_some()
}

pub fn fragments(index: u32, packet: &[u8]) -> Vec<Vec<u8>> {
    // Padding is deterministic. Omitting it makes audio useful for tiny files too.
    let len = packet
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |i| i + 1)
        .max(crate::HEADER_BYTES);
    split(index, packet, len)
}

fn split(index: u32, packet: &[u8], len: usize) -> Vec<Vec<u8>> {
    let total = len.div_ceil(PAYLOAD);
    packet[..len]
        .chunks(PAYLOAD)
        .enumerate()
        .map(|(part, data)| {
            let mut out = vec![0; BYTES];
            out[..4].copy_from_slice(b"BVA1");
            out[4..8].copy_from_slice(&index.to_le_bytes());
            out[8..10].copy_from_slice(&(len as u16).to_le_bytes());
            out[10] = part as u8;
            out[11] = total as u8;
            out[12..14].copy_from_slice(&(data.len() as u16).to_le_bytes());
            out[HEADER..HEADER + data.len()].copy_from_slice(data);
            let crc = crc32(&[&out[..14], &out[HEADER..]].concat());
            out[14..18].copy_from_slice(&crc.to_le_bytes());
            out
        })
        .collect()
}

// Fixed-size sparse slots retain out-of-order fragments without a growing RAM map.
const SLOT_BYTES: u64 = (PACKET_BYTES + 4) as u64;
pub struct Assembler {
    file: File,
    _workspace: Workspace,
}
impl Assembler {
    pub fn new() -> io::Result<Self> {
        let workspace = Workspace::new()?;
        Ok(Self {
            file: workspace.file("fragments")?,
            _workspace: workspace,
        })
    }

    pub fn accept(&mut self, fragment: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let Some((index, len, part, total, size)) = parse_fragment(fragment) else {
            return Ok(None);
        };
        let offset = index as u64 * SLOT_BYTES;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut state = [0u8; 4];
        let n = self.file.read(&mut state)?;
        if n > 0 {
            self.file.read_exact(&mut state[n..])?;
        }
        let previous_len = u16::from_le_bytes(state[..2].try_into().unwrap()) as usize;
        if previous_len != 0 && previous_len != len {
            return Ok(None);
        }
        let mut mask = u16::from_le_bytes(state[2..].try_into().unwrap());
        self.file
            .seek(SeekFrom::Start(offset + 4 + (part * PAYLOAD) as u64))?;
        self.file.write_all(&fragment[HEADER..HEADER + size])?;
        mask |= 1 << part;
        state[..2].copy_from_slice(&(len as u16).to_le_bytes());
        state[2..].copy_from_slice(&mask.to_le_bytes());
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&state)?;
        if mask != (1 << total) - 1 {
            return Ok(None);
        }
        let mut packet = vec![0u8; PACKET_BYTES];
        self.file.seek(SeekFrom::Start(offset + 4))?;
        self.file.read_exact(&mut packet[..len])?;
        // The embedded packet is independently CRC-checked by the normal decoder.
        if u32::from_le_bytes(packet[4..8].try_into().unwrap()) != index {
            return Ok(None);
        }
        Ok(Some(packet))
    }
}

fn parse_fragment(fragment: &[u8]) -> Option<(u32, usize, usize, usize, usize)> {
    if fragment.len() != BYTES || &fragment[..4] != b"BVA1" {
        return None;
    }
    let index = u32::from_le_bytes(fragment[4..8].try_into().ok()?);
    let len = u16::from_le_bytes(fragment[8..10].try_into().ok()?) as usize;
    let part = fragment[10] as usize;
    let total = fragment[11] as usize;
    let size = u16::from_le_bytes(fragment[12..14].try_into().ok()?) as usize;
    if !(crate::HEADER_BYTES..=PACKET_BYTES).contains(&len)
        || total != len.div_ceil(PAYLOAD)
        || part >= total
        || size != (len - part * PAYLOAD).min(PAYLOAD)
    {
        return None;
    }
    let crc = u32::from_le_bytes(fragment[14..18].try_into().ok()?);
    if crc32(&[&fragment[..14], &fragment[HEADER..]].concat()) != crc {
        return None;
    }
    Some((index, len, part, total, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_header_when_trailing_header_bytes_are_zero() {
        let packet = vec![0; PACKET_BYTES];
        let parts = fragments(0, &packet);
        assert_eq!(parts.len(), 1);
        assert_eq!(
            Assembler::new()
                .unwrap()
                .accept(&parts[0])
                .unwrap()
                .unwrap(),
            packet
        );
    }

    #[test]
    fn interleaves_incomplete_packets_and_cleans_disk_storage() {
        let mut first = vec![71u8; PACKET_BYTES];
        first[4..8].copy_from_slice(&0u32.to_le_bytes());
        let mut second = vec![93u8; PACKET_BYTES];
        second[4..8].copy_from_slice(&57u32.to_le_bytes());
        let a = fragments(0, &first);
        let b = fragments(57, &second);
        let mut assembler = Assembler::new().unwrap();
        let staging = assembler._workspace.0.clone();
        for i in (1..a.len()).rev() {
            assert!(assembler.accept(&a[i]).unwrap().is_none());
            assert!(assembler.accept(&b[i]).unwrap().is_none());
        }
        assert_eq!(assembler.accept(&b[0]).unwrap().unwrap(), second);
        assert_eq!(assembler.accept(&a[0]).unwrap().unwrap(), first);
        drop(assembler);
        assert!(!staging.exists());
    }

    #[test]
    fn reorders_fragments_rejects_corruption_and_restores_padding() {
        let mut packet = vec![0; PACKET_BYTES];
        for (i, byte) in packet[..1100].iter_mut().enumerate() {
            *byte = (i * 13) as u8;
        }
        packet[4..8].copy_from_slice(&42u32.to_le_bytes());
        let parts = fragments(42, &packet);
        assert!(parts.len() > 1);
        let mut assembler = Assembler::new().unwrap();
        let mut broken = parts[0].clone();
        broken[HEADER] ^= 1;
        assert!(assembler.accept(&broken).unwrap().is_none());
        for part in parts[1..].iter().rev() {
            assert!(assembler.accept(part).unwrap().is_none());
        }
        assert_eq!(assembler.accept(&parts[0]).unwrap().unwrap(), packet);
    }
}
