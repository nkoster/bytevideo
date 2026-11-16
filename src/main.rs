mod audio;
mod progress;
mod repair;
mod streaming;
use progress::Progress;
use reed_solomon_erasure::galois_8::ReedSolomon;
#[cfg(test)]
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use streaming::{Decoder, PreparedInput, Workspace};

const WIDTH: usize = 1280;
const HEIGHT: usize = 720;
const DEFAULT_TILE_SIZE: usize = 8;
#[cfg(test)]
const TILE: usize = DEFAULT_TILE_SIZE;
#[cfg(test)]
const COLS: usize = WIDTH / TILE;
#[cfg(test)]
const ROWS: usize = HEIGHT / TILE;
const FRAME_BYTES: usize = WIDTH * HEIGHT;
// Keep BV02 packet/audio sizes independent of the video tile size.
const PACKET_BYTES: usize = 1800;
#[cfg(test)]
const GRAY_LEVELS: [u8; 4] = [16, 89, 162, 235];
#[cfg(test)]
const PACKETS_PER_FRAME: usize = 2;
const HEADER_BYTES: usize = 64;
const DATA_SHARDS: usize = 16;
const PARITY_SHARDS: usize = 8;
const SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const DATA_BYTES: usize = PACKET_BYTES - HEADER_BYTES;
const COPIES: usize = 3;
const MAGIC: &[u8; 4] = b"BV02";

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (if crc & 1 != 0 { 0xedb8_8320 } else { 0 });
        }
    }
    !crc
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Metadata {
    groups: u32,
    size: u64,
    hash: [u8; 32],
}

fn codec() -> ReedSolomon {
    ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).unwrap()
}

fn make_packet(index: u32, meta: &Metadata, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0u8; PACKET_BYTES];
    packet[..4].copy_from_slice(MAGIC);
    packet[4..8].copy_from_slice(&index.to_le_bytes());
    packet[8..12].copy_from_slice(&meta.groups.to_le_bytes());
    packet[12..20].copy_from_slice(&meta.size.to_le_bytes());
    packet[20..52].copy_from_slice(&meta.hash);
    // Fixed synchronization pattern, included in the packet checksum.
    packet[52..60].copy_from_slice(b"\x55\xaa\x55\xaaBV02");
    packet[HEADER_BYTES..].copy_from_slice(payload);
    let checksum = crc32(&[&packet[..60], &packet[HEADER_BYTES..]].concat());
    packet[60..64].copy_from_slice(&checksum.to_le_bytes());
    packet
}

#[cfg(test)]
fn make_shards(data: &[u8]) -> (Metadata, Vec<Vec<Vec<u8>>>) {
    let groups = data.len().div_ceil(DATA_BYTES * DATA_SHARDS).max(1);
    let meta = Metadata {
        groups: groups as u32,
        size: data.len() as u64,
        hash: Sha256::digest(data).into(),
    };
    let mut result = Vec::with_capacity(groups);
    for group in 0..groups {
        let mut shards = vec![vec![0u8; DATA_BYTES]; SHARDS];
        for (i, shard) in shards.iter_mut().take(DATA_SHARDS).enumerate() {
            let start = (group * DATA_SHARDS + i) * DATA_BYTES;
            if start < data.len() {
                let end = (start + DATA_BYTES).min(data.len());
                shard[..end - start].copy_from_slice(&data[start..end]);
            }
        }
        codec().encode(&mut shards).unwrap();
        result.push(shards);
    }
    (meta, result)
}

// Soft Rasta tints (red, yellow, green) in YUV: chroma never changes data luma.
fn tile_chroma(level: u8, hue: usize) -> (u8, u8) {
    let (u, v) = [(-16i16, 40i16), (-40, 8), (-24, -28)][hue];
    // Reduce saturation near black/white so displayed RGB stays within gamut.
    let strength = i16::from(level.min(255 - level).min(64));
    (
        (128 + u * strength / 96) as u8,
        (128 + v * strength / 96) as u8,
    )
}

// Radix digits encode a byte using all selected brightness levels.
struct TileGeometry {
    size: usize,
    cols: usize,
    rows: usize,
    tiles_per_byte: usize,
}

impl TileGeometry {
    #[cfg(test)]
    fn new(size: usize) -> Self {
        Self::with_levels(size, 4)
    }

    fn with_levels(size: usize, levels: usize) -> Self {
        assert!((2..=10).contains(&size) && (2..=10).contains(&levels));
        let mut tiles_per_byte = 1;
        let mut values = levels;
        while values < 256 {
            values *= levels;
            tiles_per_byte += 1;
        }
        Self {
            size,
            tiles_per_byte,
            cols: WIDTH / size / tiles_per_byte * tiles_per_byte,
            rows: HEIGHT / size,
        }
    }

    fn capacity(&self) -> usize {
        self.cols * self.rows / self.tiles_per_byte
    }
    fn fragmented(&self) -> bool {
        self.capacity() < PACKET_BYTES
    }
    fn unit_bytes(&self) -> usize {
        if self.fragmented() {
            repair::BYTES
        } else {
            PACKET_BYTES
        }
    }
    fn units_per_frame(&self) -> usize {
        self.capacity() / self.unit_bytes()
    }
    fn units_per_packet(&self) -> usize {
        if self.fragmented() {
            repair::FULL_PARTS
        } else {
            1
        }
    }
    #[cfg(test)]
    fn packets_per_frame(&self) -> usize {
        self.units_per_frame()
    }
}

fn gray_level(symbol: usize, levels: usize) -> u8 {
    (16 + (symbol * 219 + (levels - 1) / 2) / (levels - 1)) as u8
}

fn color_hue(position: usize, levels: usize) -> usize {
    if levels == 2 {
        [0, 2][position % 2]
    } else {
        position % 3
    }
}

struct BytePattern {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

struct ByteLayout {
    y: usize,
    uv: usize,
    palette: usize,
    uv_rows: usize,
}

struct FrameRenderer {
    color: bool,
    tile_size: usize,
    patterns: Vec<BytePattern>,
    layouts: Vec<ByteLayout>,
    frame: Vec<u8>,
}

impl FrameRenderer {
    #[cfg(test)]
    fn new(color: bool) -> Self {
        Self::with_tile_size(color, DEFAULT_TILE_SIZE)
    }
    #[cfg(test)]
    fn with_tile_size(color: bool, tile_size: usize) -> Self {
        Self::with_levels(color, tile_size, 4)
    }

    fn with_levels(color: bool, tile_size: usize, levels: usize) -> Self {
        let geometry = TileGeometry::with_levels(tile_size, levels);
        let hues = if !color {
            1
        } else if levels == 2 {
            2
        } else {
            3
        };
        let width = tile_size * geometry.tiles_per_byte;
        let mut patterns = Vec::with_capacity(hues * 256 * 2);
        for hue in 0..hues {
            for byte in 0..256usize {
                for phase in 0..2usize {
                    let uv_width = (phase + width).div_ceil(2) - phase.div_ceil(2);
                    let mut pattern = BytePattern {
                        y: vec![0; width],
                        u: vec![128; uv_width],
                        v: vec![128; uv_width],
                    };
                    let mut divisor = levels.pow((geometry.tiles_per_byte - 1) as u32);
                    for tile in 0..geometry.tiles_per_byte {
                        let symbol = byte / divisor % levels;
                        divisor /= levels;
                        let level = gray_level(symbol, levels);
                        let luma = if color {
                            (16 + (u16::from(level) * 219 + 127) / 255) as u8
                        } else {
                            level
                        };
                        pattern.y[tile * tile_size..(tile + 1) * tile_size].fill(luma);
                        if color {
                            let (u, v) = tile_chroma(level, color_hue(hue + tile, levels));
                            let start = (phase + tile * tile_size).div_ceil(2) - phase.div_ceil(2);
                            let end =
                                (phase + (tile + 1) * tile_size).div_ceil(2) - phase.div_ceil(2);
                            pattern.u[start..end].fill(u);
                            pattern.v[start..end].fill(v);
                        }
                    }
                    patterns.push(pattern);
                }
            }
        }
        let layouts = (0..geometry.units_per_frame() * geometry.unit_bytes())
            .map(|byte| {
                let row = byte / (geometry.cols / geometry.tiles_per_byte);
                let column =
                    byte % (geometry.cols / geometry.tiles_per_byte) * geometry.tiles_per_byte;
                let x = column * tile_size;
                ByteLayout {
                    y: row * tile_size * WIDTH + x,
                    uv: (row * tile_size).div_ceil(2) * (WIDTH / 2) + x.div_ceil(2),
                    uv_rows: ((row + 1) * tile_size).div_ceil(2) - (row * tile_size).div_ceil(2),
                    palette: if color {
                        ((column + row) % hues) * 512 + x % 2
                    } else {
                        x % 2
                    },
                }
            })
            .collect();
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
            color,
            tile_size,
            patterns,
            layouts,
            frame,
        }
    }

    fn render(&mut self, packets: &[u8]) -> &[u8] {
        assert_eq!(packets.len(), self.layouts.len());
        for (&byte, layout) in packets.iter().zip(&self.layouts) {
            let pattern = &self.patterns[layout.palette + byte as usize * 2];
            for dy in 0..self.tile_size {
                let start = layout.y + dy * WIDTH;
                self.frame[start..start + pattern.y.len()].copy_from_slice(&pattern.y);
            }
            if self.color {
                for dy in 0..layout.uv_rows {
                    let start = FRAME_BYTES + layout.uv + dy * (WIDTH / 2);
                    self.frame[start..start + pattern.u.len()].copy_from_slice(&pattern.u);
                    let start = start + FRAME_BYTES / 4;
                    self.frame[start..start + pattern.v.len()].copy_from_slice(&pattern.v);
                }
            }
        }
        &self.frame
    }
}

#[cfg(test)]
fn packets_to_frame(packets: &[u8], color: bool) -> Vec<u8> {
    FrameRenderer::new(color).render(packets).to_vec()
}

// Two ordinary BV02 packets share a frame, with two bits per tile.
#[cfg(test)]
fn reference_frame(packets: &[u8], color: bool) -> Vec<u8> {
    assert_eq!(packets.len(), PACKET_BYTES * PACKETS_PER_FRAME);
    let chroma_bytes = FRAME_BYTES / 4;
    let mut frame = vec![
        128u8;
        if color {
            FRAME_BYTES + 2 * chroma_bytes
        } else {
            FRAME_BYTES
        }
    ];
    for tile in 0..COLS * ROWS {
        let symbol = (packets[tile / 4] >> (6 - 2 * (tile % 4))) & 3;
        let level = GRAY_LEVELS[symbol as usize];
        // gray input is full-range; yuv420p is limited-range. Match the existing
        // FFmpeg gray -> YUV mapping so the unchanged decoder sees the same levels.
        let luma = if color {
            (16 + (u16::from(level) * 219 + 127) / 255) as u8
        } else {
            level
        };
        let x = (tile % COLS) * TILE;
        let y = (tile / COLS) * TILE;
        for dy in 0..TILE {
            frame[(y + dy) * WIDTH + x..(y + dy) * WIDTH + x + TILE].fill(luma);
        }
        if color {
            let hue = (tile % COLS + tile / COLS) % 3;
            let (u, v) = tile_chroma(level, hue);
            for (plane, value) in [(FRAME_BYTES, u), (FRAME_BYTES + chroma_bytes, v)] {
                for dy in 0..TILE / 2 {
                    let start = plane + (y / 2 + dy) * (WIDTH / 2) + x / 2;
                    frame[start..start + TILE / 2].fill(value);
                }
            }
        }
    }
    frame
}

#[cfg(test)]
fn frame_to_packets(frame: &[u8], four_gray: bool) -> Vec<u8> {
    frame_to_packets_sized(frame, four_gray, DEFAULT_TILE_SIZE)
}

#[cfg(test)]
fn frame_to_packets_sized(frame: &[u8], four_gray: bool, tile_size: usize) -> Vec<u8> {
    frame_to_bytes(frame, tile_size, if four_gray { 4 } else { 2 })
}

fn frame_to_bytes(frame: &[u8], tile_size: usize, levels: usize) -> Vec<u8> {
    let geometry = TileGeometry::with_levels(tile_size, levels);
    let mut bytes = vec![0u8; geometry.units_per_frame() * geometry.unit_bytes()];
    let margin = geometry.size / 4;
    let samples = (geometry.size - 2 * margin).pow(2);
    let thresholds: Vec<_> = (0..levels - 1)
        .map(|i| {
            samples * (usize::from(gray_level(i, levels)) + usize::from(gray_level(i + 1, levels)))
        })
        .collect();
    for (byte_index, byte) in bytes.iter_mut().enumerate() {
        let mut value = 0usize;
        for digit in 0..geometry.tiles_per_byte {
            let tile = byte_index * geometry.tiles_per_byte + digit;
            let x = (tile % geometry.cols) * geometry.size;
            let y = (tile / geometry.cols) * geometry.size;
            let mut sum = 0usize;
            for dy in margin..geometry.size - margin {
                for dx in margin..geometry.size - margin {
                    sum += frame[(y + dy) * WIDTH + x + dx] as usize;
                }
            }
            let symbol = thresholds
                .iter()
                .filter(|&&threshold| sum * 2 >= threshold)
                .count();
            value = value * levels + symbol;
        }
        // Unused radix codewords fail the enclosing CRC like other damaged bytes.
        *byte = u8::try_from(value).unwrap_or(0);
    }
    bytes
}

fn parse_packet(packet: &[u8]) -> Option<(u32, Metadata, Vec<u8>)> {
    if packet.len() != PACKET_BYTES
        || &packet[..4] != MAGIC
        || &packet[52..60] != b"\x55\xaa\x55\xaaBV02"
    {
        return None;
    }
    let index = u32::from_le_bytes(packet[4..8].try_into().ok()?);
    let groups = u32::from_le_bytes(packet[8..12].try_into().ok()?);
    let size = u64::from_le_bytes(packet[12..20].try_into().ok()?);
    let expected_groups = size.div_ceil((DATA_BYTES * DATA_SHARDS) as u64).max(1);
    if groups as u64 != expected_groups || groups.checked_mul(SHARDS as u32)? <= index {
        return None;
    }
    let expected = u32::from_le_bytes(packet[60..64].try_into().ok()?);
    if crc32(&[&packet[..60], &packet[HEADER_BYTES..]].concat()) != expected {
        return None;
    }
    Some((
        index,
        Metadata {
            groups,
            size,
            hash: packet[20..52].try_into().ok()?,
        },
        packet[HEADER_BYTES..].to_vec(),
    ))
}

#[cfg(test)]
fn recover(meta: &Metadata, packets: &BTreeMap<u32, Vec<u8>>) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    for group in 0..meta.groups {
        let mut shards: Vec<Option<Vec<u8>>> = (0..SHARDS)
            .map(|i| packets.get(&(group * SHARDS as u32 + i as u32)).cloned())
            .collect();
        codec().reconstruct_data(&mut shards).map_err(|e| {
            io::Error::other(format!(
                "cannot recover group {group}: {e}; output not written"
            ))
        })?;
        for shard in shards.into_iter().take(DATA_SHARDS) {
            data.extend_from_slice(&shard.unwrap());
        }
    }
    data.truncate(usize::try_from(meta.size).map_err(io::Error::other)?);
    if data.len() as u64 != meta.size || <[u8; 32]>::from(Sha256::digest(&data)) != meta.hash {
        return Err(io::Error::other(
            "file SHA-256 mismatch; output not written",
        ));
    }
    Ok(data)
}

#[cfg(test)]
fn insert_packet(
    packet: &[u8],
    metadata: &mut Option<Metadata>,
    packets: &mut BTreeMap<u32, Vec<u8>>,
) {
    if let Some((index, meta, payload)) = parse_packet(packet) {
        if metadata.is_none() {
            *metadata = Some(meta.clone());
        }
        if metadata.as_ref() == Some(&meta) {
            packets.entry(index).or_insert(payload);
        }
    }
}

fn decode_audio(input: &str, decoder: &mut Decoder, compact: bool) -> io::Result<bool> {
    let mut assembler = if compact {
        match repair::Assembler::new() {
            Ok(assembler) => Some(assembler),
            Err(e) => {
                decoder.fail(e);
                return Ok(false);
            }
        }
    } else {
        None
    };
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-i",
            input,
            "-map",
            "0:a:0",
            "-vn",
            "-ac",
            "1",
            "-ar",
            &audio::RATE.to_string(),
            "-f",
            "f32le",
            "-",
        ])
        // FFmpeg must not read or change the user's terminal. Early stopping
        // can kill it before it restores terminal echo/canonical mode.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut recognised = false;
    let reader = progress::AudioReader::new(io::BufReader::new(child.stdout.take().unwrap()));
    let result = audio::read_packets_until(
        reader,
        if compact { repair::BYTES } else { PACKET_BYTES },
        |packet| {
            if compact {
                match assembler.as_mut().unwrap().accept(packet) {
                    Ok(Some(packet)) => {
                        recognised = true;
                        decoder.accept(&packet)
                    }
                    Ok(None) => false,
                    Err(e) => {
                        decoder.fail(e);
                        true
                    }
                }
            } else {
                decoder.accept(packet)
            }
        },
    );
    let complete = decoder.stopped();
    if result.is_err() || complete {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    if !status.success() && !complete {
        return Err(io::Error::other("audio unavailable or decoding failed"));
    }
    Ok(recognised)
}

const FPS: usize = 30;

struct Timeline {
    frames: u64,
    samples: u64,
}

#[cfg(test)]
fn timeline(groups: usize, full_audio: bool) -> io::Result<Timeline> {
    timeline_sized(groups, full_audio, PACKETS_PER_FRAME)
}

#[cfg(test)]
fn timeline_sized(
    groups: usize,
    full_audio: bool,
    packets_per_frame: usize,
) -> io::Result<Timeline> {
    timeline_transport(groups, full_audio, packets_per_frame, 1)
}

fn timeline_transport(
    groups: usize,
    full_audio: bool,
    units_per_frame: usize,
    units_per_packet: usize,
) -> io::Result<Timeline> {
    let samples = (groups as u64)
        .checked_mul(SHARDS as u64)
        .and_then(|n| n.checked_mul(audio::packet_samples(PACKET_BYTES) as u64))
        .ok_or_else(|| io::Error::other("media duration overflow"))?;
    let frames = if full_audio {
        samples
            .checked_mul(FPS as u64)
            .ok_or_else(|| io::Error::other("media duration overflow"))?
            .div_ceil(audio::RATE as u64)
    } else {
        (groups as u64 * SHARDS as u64 * COPIES as u64 * units_per_packet as u64)
            .div_ceil(units_per_frame as u64)
    };
    // RATE is divisible by FPS: pad audio by less than one video frame.
    Ok(Timeline {
        frames,
        samples: frames * (audio::RATE / FPS) as u64,
    })
}

fn encode(
    input: &str,
    output: &str,
    full_audio: bool,
    color: bool,
    tile_size: usize,
    gray_levels: usize,
) -> io::Result<()> {
    let geometry = TileGeometry::with_levels(tile_size, gray_levels);
    // Closer brightness levels need less quantization even before transcoding.
    let crf = 16usize
        .saturating_sub(gray_levels.saturating_sub(4) * 2)
        .to_string();
    let units_per_frame = geometry.units_per_frame();
    let units_per_packet = geometry.units_per_packet();
    let mut prepared = PreparedInput::new(Path::new(input))?;
    let groups = prepared.metadata.groups;
    let size = prepared.metadata.size;
    let timeline = timeline_transport(
        groups as usize,
        full_audio,
        units_per_frame,
        units_per_packet,
    )?;
    let input_rate = if full_audio {
        format!(
            "{}/{}",
            COPIES * audio::RATE * units_per_packet,
            units_per_frame * audio::packet_samples(PACKET_BYTES)
        )
    } else {
        FPS.to_string()
    };
    let video_filter = format!(
        "fps={FPS}:round=up,tpad=stop_mode=clone:stop_duration=1,trim=end_frame={}",
        timeline.frames
    );
    let temp = Workspace::new()?;
    let audio_path = temp.0.join("audio.f32le");
    {
        let mut writer = io::BufWriter::new(fs::File::create(&audio_path)?);
        let mut written = 0u64;
        let mut progress = Progress::new("Preparing audio", Some(timeline.samples));
        if full_audio {
            for shard in 0..SHARDS {
                for group in 0..groups {
                    audio::write_packet(&mut writer, &prepared.packet(group, shard)?)?;
                    written += audio::packet_samples(PACKET_BYTES) as u64;
                    progress.update(written, Some(timeline.samples));
                }
            }
        } else {
            // Spend a fixed audio budget on parity shards first. Complete fragments
            // only: no truncated FSK symbols or packets at the end of the video.
            let mut budget = timeline.samples / audio::packet_samples(repair::BYTES) as u64;
            'packets: for shard in (DATA_SHARDS..SHARDS).chain(0..DATA_SHARDS) {
                for group in 0..groups {
                    let index = group * SHARDS as u32 + shard as u32;
                    let parts = repair::fragments(index, &prepared.packet(group, shard)?);
                    if parts.len() as u64 > budget {
                        continue;
                    }
                    for part in parts {
                        audio::write_packet(&mut writer, &part)?;
                        written += audio::packet_samples(repair::BYTES) as u64;
                        progress.update(written, Some(timeline.samples));
                        budget -= 1;
                    }
                    if budget == 0 {
                        break 'packets;
                    }
                }
            }
        }
        // Write silence in chunks so preparation progress also covers padding.
        let silence = [0u8; 65536];
        while written < timeline.samples {
            let samples = (timeline.samples - written).min((silence.len() / 4) as u64);
            writer.write_all(&silence[..samples as usize * 4])?;
            written += samples;
            progress.update(written, Some(timeline.samples));
        }
        writer.flush()?;
        progress.finish();
    }
    let video_workspace = Workspace::beside(Path::new(output))?;
    let staged_video = video_workspace.0.join("video.mp4");
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-nostats",
            "-progress",
            "pipe:1",
            "-v",
            "error",
            "-f",
            "rawvideo",
            "-pixel_format",
            if color { "yuv420p" } else { "gray" },
            "-video_size",
            "1280x720",
            "-framerate",
            &input_rate,
            "-i",
            "-",
            "-f",
            "f32le",
            "-ar",
            &audio::RATE.to_string(),
            "-ac",
            "1",
            "-i",
            audio_path
                .to_str()
                .ok_or_else(|| io::Error::other("invalid temporary path"))?,
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-vf",
            &video_filter,
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            &crf,
            "-pix_fmt",
            "yuv420p",
            // MP4's default 1 kHz movie clock rounds AAC edit durations to
            // milliseconds. Use the audio clock so both tracks end sample-exact.
            "-movie_timescale",
            &audio::RATE.to_string(),
            "-movflags",
            "+faststart",
            staged_video
                .to_str()
                .ok_or_else(|| io::Error::other("invalid output path"))?,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let progress_output = child.stdout.take().unwrap();
    let total_frames = timeline.frames;
    let progress_thread = std::thread::spawn(move || {
        let mut progress = Progress::new("Encoding video ", Some(total_frames));
        for line in io::BufReader::new(progress_output).lines() {
            let Ok(line) = line else { break };
            if let Some(frame) = line
                .strip_prefix("frame=")
                .and_then(|v| v.trim().parse().ok())
            {
                progress.update(frame, Some(total_frames));
            }
        }
        progress
    });
    let result = (|| -> io::Result<()> {
        let mut pipe = child.stdin.take().unwrap();
        // Send the same shard position from each group before the next position.
        // Copies are separate passes, so a short burst cannot erase all copies.
        let mut pending = Vec::with_capacity(geometry.unit_bytes() * units_per_frame);
        let mut renderer = FrameRenderer::with_levels(color, tile_size, gray_levels);
        for _ in 0..COPIES {
            for shard in 0..SHARDS {
                for group in 0..groups {
                    let packet = prepared.packet(group, shard)?;
                    let units = if geometry.fragmented() {
                        repair::full_fragments(group * SHARDS as u32 + shard as u32, &packet)
                    } else {
                        vec![packet]
                    };
                    for unit in units {
                        pending.extend_from_slice(&unit);
                        if pending.len() == geometry.unit_bytes() * units_per_frame {
                            pipe.write_all(renderer.render(&pending))?;
                            pending.clear();
                        }
                    }
                }
            }
        }
        if !pending.is_empty() {
            pending.resize(geometry.unit_bytes() * units_per_frame, 0);
            pipe.write_all(renderer.render(&pending))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    let mut progress = progress_thread
        .join()
        .map_err(|_| io::Error::other("progress thread failed"))?;
    result?;
    if !status?.success() {
        return Err(io::Error::other("ffmpeg encoding failed"));
    }
    fs::File::open(&staged_video)?.sync_all()?;
    fs::rename(&staged_video, output)?;
    progress.finish();
    println!(
        "Encoded {} bytes into {} frames ({:.3} seconds, audio and video)",
        size,
        timeline.frames,
        timeline.samples as f64 / audio::RATE as f64
    );
    Ok(())
}

fn decode(input: &str, output: &str) -> io::Result<()> {
    let mut decoder = Decoder::new(Path::new(output))?;
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-i",
            input,
            "-map",
            "0:v:0",
            "-an",
            "-fps_mode",
            "passthrough",
            "-vf",
            "scale=1280:720",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray",
            "-",
        ])
        // FFmpeg must not read or change the user's terminal. Early stopping
        // can kill it before it restores terminal echo/canonical mode.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut reader = child.stdout.take().unwrap();
    let mut frames = 0usize;
    let mut detected_format = None;
    let mut video_assembler = repair::Assembler::new()?;
    let mut frame = vec![0u8; FRAME_BYTES];
    let mut progress = Progress::new("Decoding video", None);
    let video_result = (|| -> io::Result<()> {
        loop {
            let mut filled = 0;
            while filled < FRAME_BYTES {
                let n = reader.read(&mut frame[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            if filled != FRAME_BYTES {
                return Err(io::Error::other("partial decoded frame"));
            }
            frames += 1;
            // Packet/fragment CRCs identify both tile size and brightness alphabet.
            let candidates: Vec<_> = if let Some(format) = detected_format {
                vec![format]
            } else {
                std::iter::once(DEFAULT_TILE_SIZE)
                    .chain((2..=10).filter(|&size| size != DEFAULT_TILE_SIZE))
                    .flat_map(|size| [2, 4, 3, 5, 6, 7, 8, 9, 10].map(|levels| (size, levels)))
                    .collect()
            };
            for (tile_size, levels) in candidates {
                let geometry = TileGeometry::with_levels(tile_size, levels);
                let packed = frame_to_bytes(&frame, tile_size, levels);
                for unit in packed.chunks(geometry.unit_bytes()) {
                    if geometry.fragmented() {
                        if repair::valid_fragment(unit) {
                            detected_format = Some((tile_size, levels));
                            if let Some(packet) = video_assembler.accept(unit)? {
                                decoder.accept(&packet);
                            }
                        }
                    } else {
                        if detected_format.is_none() && parse_packet(unit).is_some() {
                            detected_format = Some((tile_size, levels));
                        }
                        decoder.accept(unit);
                    }
                    if decoder.stopped() {
                        break;
                    }
                }
                if decoder.stopped() || detected_format.is_some() {
                    break;
                }
            }
            let total = decoder
                .metadata
                .as_ref()
                .map(|m| m.groups as u64 * DATA_SHARDS as u64);
            progress.update(decoder.useful, total);
            if decoder.stopped() {
                break;
            }
        }
        Ok(())
    })();
    if decoder.verified {
        progress.finish();
    }
    drop(progress);
    let complete = decoder.stopped();
    // A verified early stop deliberately closes the pipe. Reap ffmpeg and do not
    // report its expected SIGPIPE/termination as a decoding error.
    if video_result.is_err() || complete {
        let _ = child.kill();
    }
    drop(reader);
    let status = child.wait()?;
    decoder.check_error()?;
    let mut audio_used = false;
    if !complete {
        audio_used = true;
        if !status.success() || video_result.is_err() {
            eprintln!("Video incomplete; trying audio repair");
        }
        let compact = decode_audio(input, &mut decoder, true).unwrap_or(false);
        decoder.check_error()?;
        // Old BV02 files and --full-audio use the original full-packet modem.
        if !compact && !decoder.stopped() {
            let _ = decode_audio(input, &mut decoder, false);
            decoder.check_error()?;
        }
    }
    let size = decoder.finish(Path::new(output))?;
    println!(
        "Recovered {} bytes into {} ({} video frames read; audio {})",
        size,
        output,
        frames,
        if audio_used { "used" } else { "skipped" }
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct EncodeOptions {
    full_audio: bool,
    color: bool,
    tile_size: usize,
    gray_levels: usize,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            full_audio: false,
            color: false,
            tile_size: DEFAULT_TILE_SIZE,
            gray_levels: 2,
        }
    }
}

fn encode_options(flags: &[String]) -> io::Result<EncodeOptions> {
    let mut options = EncodeOptions::default();
    let mut flags = flags.iter();
    let mut tile_size_seen = false;
    let mut gray_levels_seen = false;
    while let Some(flag) = flags.next() {
        if flag == "--tile-size" || flag.starts_with("--tile-size=") {
            if tile_size_seen {
                return Err(io::Error::other("duplicate encode option: --tile-size"));
            }
            tile_size_seen = true;
            let value = if let Some((_, value)) = flag.split_once('=') {
                value
            } else {
                flags.next().map(String::as_str).unwrap_or("")
            };
            options.tile_size = value
                .parse::<usize>()
                .ok()
                .filter(|size| (2..=10).contains(size))
                .ok_or_else(|| io::Error::other("--tile-size requires an integer from 2 to 10"))?;
            continue;
        }
        if flag == "--gray-levels" || flag.starts_with("--gray-levels=") {
            if gray_levels_seen {
                return Err(io::Error::other("duplicate encode option: --gray-levels"));
            }
            gray_levels_seen = true;
            let value = if let Some((_, value)) = flag.split_once('=') {
                value
            } else {
                flags.next().map(String::as_str).unwrap_or("")
            };
            options.gray_levels = value
                .parse::<usize>()
                .ok()
                .filter(|levels| (2..=10).contains(levels))
                .ok_or_else(|| {
                    io::Error::other("--gray-levels requires an integer from 2 to 10")
                })?;
            continue;
        }
        let enabled = match flag.as_str() {
            "--full-audio" => &mut options.full_audio,
            "--color" => &mut options.color,
            _ => return Err(io::Error::other(format!("unknown encode option: {flag}"))),
        };
        if *enabled {
            return Err(io::Error::other(format!("duplicate encode option: {flag}")));
        }
        *enabled = true;
    }
    Ok(options)
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: bytevideo encode input.bin output.mp4 [--full-audio] [--color] [--tile-size 2..10] [--gray-levels 2..10]\n       bytevideo decode input.mp4 output.bin");
        std::process::exit(2);
    }
    match args[1].as_str() {
        "encode" => {
            let options = encode_options(&args[4..])?;
            encode(
                &args[2],
                &args[3],
                options.full_audio,
                options.color,
                options.tile_size,
                options.gray_levels,
            )
        }
        "decode" if args.len() == 4 => decode(&args[2], &args[3]),
        "decode" => Err(io::Error::other(
            "decode does not accept options; tile size and gray levels are detected automatically",
        )),
        _ => Err(io::Error::other("expected encode or decode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(data: &[u8]) -> (Metadata, BTreeMap<u32, Vec<u8>>) {
        let (meta, groups) = make_shards(data);
        let packets = groups
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, shard)| (i as u32, shard))
            .collect();
        (meta, packets)
    }

    #[test]
    fn restores_eight_missing_shards_per_group() {
        let data: Vec<u8> = (0..DATA_BYTES * DATA_SHARDS + 317)
            .map(|i| i as u8)
            .collect();
        let (meta, mut packets) = packets(&data);
        packets.retain(|index, _| index % SHARDS as u32 >= PARITY_SHARDS as u32);
        assert_eq!(recover(&meta, &packets).unwrap(), data);
    }

    #[test]
    fn rejects_excess_loss_and_wrong_hash() {
        let (mut meta, mut packets) = packets(b"test");
        meta.hash[0] ^= 1;
        assert!(recover(&meta, &packets).is_err());
        packets.retain(|index, _| *index > PARITY_SHARDS as u32);
        assert!(recover(&meta, &packets).is_err());
    }

    #[test]
    fn combines_incomplete_audio_and_video_packets() {
        let data = b"both channels are required";
        let (meta, groups) = make_shards(data);
        let mut video = BTreeMap::new();
        let mut audio_packets = BTreeMap::new();
        let mut metadata = None;
        for (shard, payload) in groups[0].iter().enumerate() {
            let packet = make_packet(shard as u32, &meta, payload);
            if shard < 12 {
                insert_packet(&packet, &mut metadata, &mut video);
            } else {
                let mut pcm = Vec::new();
                audio::write_packet(&mut pcm, &packet).unwrap();
                audio::read_packets(&pcm[..], PACKET_BYTES, |p| {
                    insert_packet(p, &mut metadata, &mut audio_packets)
                })
                .unwrap();
            }
        }
        assert!(recover(&meta, &video).is_err());
        assert!(recover(&meta, &audio_packets).is_err());
        video.extend(audio_packets);
        assert_eq!(recover(&meta, &video).unwrap(), data);
    }

    #[test]
    fn audio_and_video_share_an_exact_timeline() {
        for groups in [1, 2, 3, 100, 100_000] {
            let t = timeline(groups, true).unwrap();
            let payload =
                groups as u64 * SHARDS as u64 * audio::packet_samples(PACKET_BYTES) as u64;
            assert_eq!(t.frames * audio::RATE as u64, t.samples * FPS as u64);
            assert!(t.samples >= payload);
            assert!(t.samples - payload < (audio::RATE / FPS) as u64);
        }
        assert_eq!(timeline(1, true).unwrap().frames, 5208);
    }

    #[test]
    fn fast_timeline_and_audio_budget() {
        for groups in [1, 3, 4, 100] {
            let t = timeline(groups, false).unwrap();
            assert_eq!(
                t.frames,
                (groups * SHARDS * COPIES / PACKETS_PER_FRAME) as u64
            );
            assert_eq!(t.samples * FPS as u64, t.frames * audio::RATE as u64);
        }
        assert_eq!(timeline(1, false).unwrap().frames, 36);
    }

    #[test]
    fn decoder_only_stops_after_verified_reconstruction() {
        let (mut meta, groups) = make_shards(b"verified");
        let work = Workspace::new().unwrap();
        let output = work.0.join("restored");
        let mut decoder = Decoder::new(&output).unwrap();
        for (i, payload) in groups[0].iter().enumerate().take(DATA_SHARDS - 1) {
            assert!(!decoder.accept(&make_packet(i as u32, &meta, payload)));
        }
        assert!(decoder.accept(&make_packet(15, &meta, &groups[0][15])));
        assert!(decoder.verified);
        decoder.finish(&output).unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"verified");
        meta.hash[0] ^= 1;
        let mut decoder = Decoder::new(&output).unwrap();
        for (i, payload) in groups[0].iter().enumerate() {
            if decoder.accept(&make_packet(i as u32, &meta, payload)) {
                break;
            }
        }
        assert!(!decoder.verified);
        assert!(decoder.finish(&output).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"verified");
    }

    #[test]
    fn empty_file_roundtrip() {
        let (meta, packets) = packets(b"");
        assert_eq!(recover(&meta, &packets).unwrap(), b"");
    }

    #[test]
    fn renderer_matches_original_bytes_and_reuses_its_buffer() {
        for color in [false, true] {
            let mut renderer = FrameRenderer::new(color);
            let address = renderer.frame.as_ptr();
            let capacity = renderer.frame.capacity();
            for phase in [0usize, 1, 83, 255] {
                let packed: Vec<u8> = (0..PACKET_BYTES * PACKETS_PER_FRAME)
                    .map(|i| (i * 71 + phase) as u8)
                    .collect();
                assert_eq!(renderer.render(&packed), reference_frame(&packed, color));
                assert_eq!(renderer.frame.as_ptr(), address);
                assert_eq!(renderer.frame.capacity(), capacity);
            }
        }
    }

    #[test]
    #[ignore = "manual frame rendering benchmark, excludes FFmpeg"]
    fn render_benchmark() {
        use std::hint::black_box;
        let frames: usize = env::var("RENDER_FRAMES")
            .unwrap_or_else(|_| "5000".into())
            .parse()
            .unwrap();
        let reuse = env::var("RENDER_MODE").unwrap() == "reuse";
        let color = env::var("RENDER_COLOR").unwrap_or_else(|_| "1".into()) == "1";
        let mut packets: Vec<u8> = (0..PACKET_BYTES * PACKETS_PER_FRAME)
            .map(|i| (i * 71) as u8)
            .collect();
        let mut renderer = FrameRenderer::new(color);
        let started = std::time::Instant::now();
        for i in 0..frames {
            packets[0] = i as u8;
            if reuse {
                black_box(renderer.render(black_box(&packets)));
            } else {
                black_box(reference_frame(black_box(&packets), color));
            }
        }
        println!(
            "render mode={} color={} frames={} seconds={:.6}",
            if reuse { "reuse" } else { "reference" },
            color,
            frames,
            started.elapsed().as_secs_f64()
        );
    }

    #[test]
    fn color_preserves_luma_data_and_uses_three_tints() {
        let packed = vec![0b00011011; PACKET_BYTES * PACKETS_PER_FRAME];
        let gray = packets_to_frame(&packed, false);
        let color = packets_to_frame(&packed, true);
        assert_eq!(color.len(), FRAME_BYTES * 3 / 2);
        let restored: Vec<u8> = color[..FRAME_BYTES]
            .iter()
            .map(|&y| ((u16::from(y) - 16) * 255 + 109).div_euclid(219) as u8)
            .collect();
        assert_eq!(frame_to_packets(&restored, true), packed);
        for (expected, actual) in gray.iter().zip(&restored) {
            assert!(expected.abs_diff(*actual) <= 1);
        }
        for tile in 0..12 {
            let level = GRAY_LEVELS[tile % 4];
            let hue = tile % 3;
            let (u, v) = tile_chroma(level, hue);
            assert_eq!(color[FRAME_BYTES + tile * TILE / 2], u);
            assert_eq!(color[FRAME_BYTES + FRAME_BYTES / 4 + tile * TILE / 2], v);
            match hue {
                0 => assert!(u < 128 && v > 128),
                1 => assert!(u < 128 && v > 128 && 128 - u > v - 128),
                _ => assert!(u < 128 && v < 128),
            }
        }
    }

    #[test]
    fn all_tile_sizes_preserve_data_in_gray_and_color() {
        for size in 2..=10 {
            let geometry = TileGeometry::new(size);
            let packed: Vec<u8> = (0..PACKET_BYTES * geometry.packets_per_frame())
                .map(|i| (i * 71 + i / 256) as u8)
                .collect();
            for color in [false, true] {
                let mut renderer = FrameRenderer::with_tile_size(color, size);
                let frame = renderer.render(&packed);
                let gray: Vec<_> = frame[..FRAME_BYTES]
                    .iter()
                    .map(|&y| {
                        if color {
                            ((u16::from(y) - 16) * 255 + 109) / 219
                        } else {
                            u16::from(y)
                        }
                    })
                    .map(|y| y as u8)
                    .collect();
                assert_eq!(
                    frame_to_packets_sized(&gray, true, size),
                    packed,
                    "tile size {size}, color {color}"
                );
            }
            for groups in [1, 3, 8] {
                let count = geometry.packets_per_frame();
                let t = timeline_sized(groups, false, count).unwrap();
                let packets = (groups * SHARDS * COPIES) as u64;
                assert!(t.frames * count as u64 >= packets);
                assert!((t.frames - 1) * (count as u64) < packets);
                assert_eq!(t.samples * FPS as u64, t.frames * audio::RATE as u64);
                assert_eq!(
                    timeline_sized(groups, true, count).unwrap().frames,
                    timeline(groups, true).unwrap().frames
                );
            }
        }
    }

    #[test]
    fn every_alphabet_and_tile_size_roundtrips_all_byte_values() {
        for levels in 2..=10 {
            for size in 2..=10 {
                let geometry = TileGeometry::with_levels(size, levels);
                let packed: Vec<u8> = (0..geometry.units_per_frame() * geometry.unit_bytes())
                    .map(|i| i as u8)
                    .collect();
                assert!(geometry.units_per_frame() > 0);
                for color in [false, true] {
                    let mut renderer = FrameRenderer::with_levels(color, size, levels);
                    let frame = renderer.render(&packed);
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
                        frame_to_bytes(&gray, size, levels),
                        packed,
                        "levels {levels}, tile {size}, color {color}"
                    );
                }
                let units = (3 * SHARDS * COPIES * geometry.units_per_packet()) as u64;
                let t = timeline_transport(
                    3,
                    false,
                    geometry.units_per_frame(),
                    geometry.units_per_packet(),
                )
                .unwrap();
                assert!(t.frames * geometry.units_per_frame() as u64 >= units);
                assert!((t.frames - 1) * (geometry.units_per_frame() as u64) < units);
            }
        }
    }

    #[test]
    fn binary_color_uses_only_red_and_green_without_changing_brightness() {
        let packed = vec![0b01010101; PACKET_BYTES];
        let mut renderer = FrameRenderer::with_levels(true, 8, 2);
        let frame = renderer.render(&packed);
        for row in 0..ROWS {
            for col in 0..COLS {
                let tile = row * COLS + col;
                let symbol = (packed[tile / 8] >> (7 - tile % 8)) & 1;
                let level = if symbol == 0 { 16 } else { 235 };
                let hue = if (row + col) % 2 == 0 { 0 } else { 2 };
                let (u, v) = tile_chroma(level, hue);
                let offset = row * 4 * (WIDTH / 2) + col * 4;
                assert_eq!(frame[FRAME_BYTES + offset], u);
                assert_eq!(frame[FRAME_BYTES + FRAME_BYTES / 4 + offset], v);
                assert_eq!(
                    frame[row * 8 * WIDTH + col * 8],
                    (16 + (u16::from(level) * 219 + 127) / 255) as u8
                );
            }
        }
    }

    #[test]
    fn gray_levels_options_validate_and_combine() {
        assert_eq!(EncodeOptions::default().gray_levels, 2);
        let flags = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for levels in 2..=10 {
            let value = levels.to_string();
            for items in [
                vec![
                    "--gray-levels",
                    value.as_str(),
                    "--color",
                    "--tile-size",
                    "3",
                    "--full-audio",
                ],
                vec![
                    "--full-audio",
                    "--tile-size",
                    "3",
                    "--color",
                    "--gray-levels",
                    value.as_str(),
                ],
            ] {
                assert_eq!(
                    encode_options(&flags(&items)).unwrap(),
                    EncodeOptions {
                        full_audio: true,
                        color: true,
                        tile_size: 3,
                        gray_levels: levels,
                    }
                );
            }
            assert_eq!(
                encode_options(&[format!("--gray-levels={levels}")])
                    .unwrap()
                    .gray_levels,
                levels
            );
        }
        for items in [
            vec!["--gray-levels"],
            vec!["--gray-levels="],
            vec!["--gray-levels", "1"],
            vec!["--gray-levels", "11"],
            vec!["--gray-levels", "-1"],
            vec!["--gray-levels", "3.5"],
            vec!["--gray-levels", "abc"],
            vec!["--gray-levels", "--color"],
            vec!["--gray-levels", "9999999999999999999999999999"],
            vec!["--gray-levels", "3", "--gray-levels=4"],
        ] {
            assert!(encode_options(&flags(&items)).is_err(), "{items:?}");
        }
    }

    #[test]
    fn tile_size_options_validate_values_and_combine_with_flags() {
        let flags = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for size in 2..=10 {
            let value = size.to_string();
            for items in [
                vec!["--color", "--tile-size", value.as_str(), "--full-audio"],
                vec!["--tile-size", value.as_str(), "--full-audio", "--color"],
            ] {
                assert_eq!(
                    encode_options(&flags(&items)).unwrap(),
                    EncodeOptions {
                        tile_size: size,
                        color: true,
                        full_audio: true,
                        ..EncodeOptions::default()
                    }
                );
            }
            assert_eq!(
                encode_options(&[format!("--tile-size={size}")])
                    .unwrap()
                    .tile_size,
                size
            );
        }
        for items in [
            vec!["--tile-size"],
            vec!["--tile-size="],
            vec!["--tile-size", "--color"],
            vec!["--tile-size", "1"],
            vec!["--tile-size", "11"],
            vec!["--tile-size", "-1"],
            vec!["--tile-size", "3.5"],
            vec!["--tile-size", "abc"],
            vec!["--tile-size", "999999999999999999999999"],
            vec!["--tile-size", "3", "--tile-size=4"],
        ] {
            assert!(encode_options(&flags(&items)).is_err(), "{items:?}");
        }
    }

    #[test]
    fn encode_flags_combine_in_either_order_and_reject_mistakes() {
        let flags = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(encode_options(&[]).unwrap(), EncodeOptions::default());
        for options in [["--color", "--full-audio"], ["--full-audio", "--color"]] {
            assert_eq!(
                encode_options(&flags(&options)).unwrap(),
                EncodeOptions {
                    color: true,
                    full_audio: true,
                    ..EncodeOptions::default()
                }
            );
        }
        assert!(encode_options(&flags(&["--colour"])).is_err());
        assert!(encode_options(&flags(&["--color", "--color"])).is_err());
    }

    #[test]
    fn four_gray_levels_and_brightness_tolerance() {
        // 00, 01, 10, 11 in every byte exercises all four levels equally.
        let packed = vec![0b00011011; PACKET_BYTES * PACKETS_PER_FRAME];
        let frame = packets_to_frame(&packed, false);
        for (tile, level) in GRAY_LEVELS.iter().enumerate() {
            assert_eq!(frame[tile * TILE], *level);
        }
        for offset in [-24i16, 0, 24] {
            let shifted: Vec<u8> = frame
                .iter()
                .map(|&v| (v as i16 + offset).clamp(0, 255) as u8)
                .collect();
            assert_eq!(frame_to_packets(&shifted, true), packed);
        }
    }

    #[test]
    fn decodes_legacy_binary_tiles() {
        let (meta, groups) = make_shards(b"legacy");
        let packet = make_packet(0, &meta, &groups[0][0]);
        let mut frame = vec![0; FRAME_BYTES];
        for tile in 0..COLS * ROWS {
            let value = if packet[tile / 8] & (1 << (7 - tile % 8)) != 0 {
                235
            } else {
                16
            };
            let x = tile % COLS * TILE;
            let y = tile / COLS * TILE;
            for dy in 0..TILE {
                frame[(y + dy) * WIDTH + x..(y + dy) * WIDTH + x + TILE].fill(value);
            }
        }
        assert_eq!(frame_to_packets(&frame, false), packet);
        assert!(frame_to_packets(&frame, true)
            .as_chunks::<PACKET_BYTES>()
            .0
            .iter()
            .all(|p| parse_packet(p).is_none()));
    }

    #[test]
    fn tile_noise_and_corrupt_packets() {
        let (meta, groups) = make_shards(b"tile test");
        let packet = make_packet(0, &meta, &groups[0][0]);
        let second = make_packet(1, &meta, &groups[0][1]);
        let packed = [packet.clone(), second].concat();
        let mut frame = packets_to_frame(&packed, false);
        // Change one center pixel in every tile; the average still identifies the two-bit symbol.
        for y in 0..ROWS {
            for x in 0..COLS {
                let i = (y * TILE + 3) * WIDTH + x * TILE + 3;
                frame[i] = 255 - frame[i];
            }
        }
        assert_eq!(frame_to_packets(&frame, true), packed);
        assert!(parse_packet(&packet).is_some());
        let mut corrupt = packet.clone();
        corrupt[HEADER_BYTES] ^= 1;
        assert!(parse_packet(&corrupt).is_none());
        corrupt = packet;
        corrupt[12] ^= 1;
        assert!(parse_packet(&corrupt).is_none());
        assert!(parse_packet(&[]).is_none());
    }
}
