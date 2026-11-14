//! Binary FSK transport, independent of the packet format.
use std::f32::consts::TAU;
use std::io::{self, Read, Write};

pub const RATE: usize = 48_000;
const SYMBOL: usize = 24; // 2,000 bits/s, tones at 2 and 4 kHz.
const SYNC: u64 = 0xd391_c5a7_6e28_b04f;

/// Includes the transport synchronization word; shared with video timing.
pub const fn packet_samples(packet_len: usize) -> usize {
    (8 + packet_len) * 8 * SYMBOL
}

fn tones() -> [[f32; SYMBOL]; 4] {
    std::array::from_fn(|j| {
        std::array::from_fn(|i| {
            let angle = TAU * (j / 2 + 1) as f32 * i as f32 / SYMBOL as f32;
            if j % 2 == 0 {
                angle.sin()
            } else {
                angle.cos()
            }
        })
    })
}

pub fn write_packet(mut writer: impl Write, packet: &[u8]) -> io::Result<()> {
    let tones = tones();
    for byte in SYNC.to_be_bytes().iter().chain(packet) {
        for bit in (0..8).rev() {
            let tone = &tones[if byte & (1 << bit) == 0 { 0 } else { 2 }];
            for sample in tone {
                writer.write_all(&(sample * 0.7).to_le_bytes())?;
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Phase {
    sync: u64,
    bytes: Vec<u8>,
    byte: u8,
    bits: usize,
    receiving: bool,
}

/// Examine every possible sample phase, so AAC delay or leading silence do not
/// require an externally known starting offset. The caller validates packet CRCs.
#[cfg(test)]
pub fn read_packets(
    reader: impl Read,
    packet_len: usize,
    mut accept: impl FnMut(&[u8]),
) -> io::Result<()> {
    read_packets_until(reader, packet_len, |packet| {
        accept(packet);
        false
    })
}

pub fn read_packets_until(
    mut reader: impl Read,
    packet_len: usize,
    mut accept: impl FnMut(&[u8]) -> bool,
) -> io::Result<()> {
    let tones = tones();
    let mut window = [0f32; SYMBOL];
    let mut phases: [Phase; SYMBOL] = std::array::from_fn(|_| Phase::default());
    let mut count = 0usize;
    let mut sample = [0u8; 4];
    loop {
        let mut filled = 0;
        while filled < 4 {
            let n = reader.read(&mut sample[filled..])?;
            if n == 0 {
                return if filled == 0 {
                    Ok(())
                } else {
                    Err(io::Error::other("partial audio sample"))
                };
            }
            filled += n;
        }
        window[count % SYMBOL] = f32::from_le_bytes(sample);
        count += 1;
        if count < SYMBOL {
            continue;
        }
        let mut energy = [0f32; 4];
        for (i, value) in window.iter().enumerate() {
            for j in 0..4 {
                energy[j] += value * tones[j][i];
            }
        }
        let bit =
            u8::from(energy[2].powi(2) + energy[3].powi(2) > energy[0].powi(2) + energy[1].powi(2));
        let phase = &mut phases[count % SYMBOL];
        if phase.receiving {
            phase.byte = (phase.byte << 1) | bit;
            phase.bits += 1;
            if phase.bits == 8 {
                phase.bytes.push(phase.byte);
                phase.bits = 0;
                phase.byte = 0;
                if phase.bytes.len() == packet_len {
                    if accept(&phase.bytes) {
                        return Ok(());
                    }
                    phase.bytes.clear();
                    phase.receiving = false;
                    phase.sync = 0;
                }
            }
        } else {
            phase.sync = (phase.sync << 1) | bit as u64;
            if phase.sync == SYNC {
                phase.receiving = true;
                phase.bytes.clear();
                phase.bits = 0;
                phase.byte = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_packets_after_unaligned_silence_with_gain_and_noise() {
        let packet: Vec<u8> = (0..1800).map(|i| (i * 37) as u8).collect();
        let mut pcm = Vec::new();
        write_packet(&mut pcm, &packet).unwrap();
        let mut samples = vec![0u8; 137 * 4];
        for (i, bytes) in pcm.chunks(4).enumerate() {
            let value =
                f32::from_le_bytes(bytes.try_into().unwrap()) * 0.3 + (i % 7) as f32 * 0.002;
            samples.extend_from_slice(&value.to_le_bytes());
        }
        let mut found = false;
        read_packets(&samples[..], packet.len(), |p| {
            found |= p == packet;
        })
        .unwrap();
        assert!(found);
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn survives_aac_encoding_and_decoding() {
        let packet: Vec<u8> = (0..1800).map(|i| (i * 71 + 19) as u8).collect();
        let mut pcm = Vec::new();
        write_packet(&mut pcm, &packet).unwrap();
        let mut encoder = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "f32le",
                "-ar",
                &RATE.to_string(),
                "-ac",
                "1",
                "-i",
                "-",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-f",
                "adts",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = encoder.stdin.take().unwrap();
        let writer = std::thread::spawn(move || input.write_all(&pcm).unwrap());
        let encoded = encoder.wait_with_output().unwrap();
        writer.join().unwrap();
        assert!(encoded.status.success());
        let mut decoder = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                "-",
                "-f",
                "f32le",
                "-ac",
                "1",
                "-ar",
                &RATE.to_string(),
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = decoder.stdin.take().unwrap();
        let writer = std::thread::spawn(move || input.write_all(&encoded.stdout).unwrap());
        let decoded = decoder.wait_with_output().unwrap();
        writer.join().unwrap();
        assert!(decoded.status.success());
        let mut found = false;
        read_packets(&decoded.stdout[..], packet.len(), |p| found |= p == packet).unwrap();
        assert!(found, "AAC roundtrip must preserve a complete packet");
    }
}
