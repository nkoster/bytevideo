//! Small, CRC-protected audio fragments of ordinary BV02 packets.
use crate::{crc32, PACKET_BYTES};
use std::collections::BTreeMap;

pub const BYTES: usize = 256;
const HEADER: usize = 18;
const PAYLOAD: usize = BYTES - HEADER;

pub fn fragments(index: u32, packet: &[u8]) -> Vec<Vec<u8>> {
    // Padding is deterministic. Omitting it makes audio useful for tiny files too.
    let len = packet
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |i| i + 1)
        .max(crate::HEADER_BYTES);
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

struct Pending {
    len: usize,
    parts: Vec<Option<Vec<u8>>>,
}

#[derive(Default)]
pub struct Assembler {
    pending: BTreeMap<u32, Pending>,
}
impl Assembler {
    pub fn accept(&mut self, fragment: &[u8]) -> Option<Vec<u8>> {
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
        let entry = self.pending.entry(index).or_insert_with(|| Pending {
            len,
            parts: vec![None; total],
        });
        if entry.len != len {
            return None;
        }
        entry.parts[part] = Some(fragment[HEADER..HEADER + size].to_vec());
        if entry.parts.iter().any(Option::is_none) {
            return None;
        }
        let parts = self.pending.remove(&index)?.parts;
        let mut packet: Vec<u8> = parts.into_iter().flatten().flatten().collect();
        packet.resize(PACKET_BYTES, 0);
        // The embedded packet is independently CRC-checked by the normal decoder.
        if u32::from_le_bytes(packet[4..8].try_into().ok()?) != index {
            return None;
        }
        Some(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_header_when_trailing_header_bytes_are_zero() {
        let packet = vec![0; PACKET_BYTES];
        let parts = fragments(0, &packet);
        assert_eq!(parts.len(), 1);
        assert_eq!(Assembler::default().accept(&parts[0]).unwrap(), packet);
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
        let mut assembler = Assembler::default();
        let mut broken = parts[0].clone();
        broken[HEADER] ^= 1;
        assert!(assembler.accept(&broken).is_none());
        for part in parts[1..].iter().rev() {
            assert!(assembler.accept(part).is_none());
        }
        assert_eq!(assembler.accept(&parts[0]).unwrap(), packet);
    }
}
