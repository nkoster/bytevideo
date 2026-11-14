mod audio;
mod progress;
mod repair;
use progress::Progress;
use reed_solomon_erasure::galois_8::ReedSolomon;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::process::{Command, Stdio};

const WIDTH: usize = 1280;
const HEIGHT: usize = 720;
const TILE: usize = 8;
const COLS: usize = WIDTH / TILE;
const ROWS: usize = HEIGHT / TILE;
const FRAME_BYTES: usize = WIDTH * HEIGHT;
const PACKET_BYTES: usize = COLS * ROWS / 8;
const GRAY_LEVELS: [u8; 4] = [16, 89, 162, 235];
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

// Two ordinary BV02 packets share a frame, with two bits per tile.
fn packets_to_frame(packets: &[u8]) -> Vec<u8> {
    assert_eq!(packets.len(), PACKET_BYTES * PACKETS_PER_FRAME);
    let mut frame = vec![0u8; FRAME_BYTES];
    for tile in 0..COLS * ROWS {
        let symbol = (packets[tile / 4] >> (6 - 2 * (tile % 4))) & 3;
        let x = (tile % COLS) * TILE;
        let y = (tile / COLS) * TILE;
        for dy in 0..TILE {
            frame[(y + dy) * WIDTH + x..(y + dy) * WIDTH + x + TILE]
                .fill(GRAY_LEVELS[symbol as usize]);
        }
    }
    frame
}

fn frame_to_packets(frame: &[u8], four_gray: bool) -> Vec<u8> {
    let bits = if four_gray { 2 } else { 1 };
    let mut packets = vec![0u8; PACKET_BYTES * bits];
    for tile in 0..COLS * ROWS {
        let x = (tile % COLS) * TILE;
        let y = (tile / COLS) * TILE;
        let mut sum = 0usize;
        for dy in 2..6 {
            for dx in 2..6 {
                sum += frame[(y + dy) * WIDTH + x + dx] as usize;
            }
        }
        let symbol = if four_gray {
            // Compare the unrounded average with the midpoints between levels.
            GRAY_LEVELS
                .windows(2)
                .filter(|pair| sum * 2 >= 16 * (pair[0] as usize + pair[1] as usize))
                .count() as u8
        } else {
            u8::from(sum >= 16 * 128)
        };
        let bit = tile * bits;
        packets[bit / 8] |= symbol << (8 - bits - bit % 8);
    }
    packets
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

struct TempAudio(std::path::PathBuf);
impl TempAudio {
    fn new() -> io::Result<Self> {
        let base = env::temp_dir();
        for serial in 0..1000 {
            let path = base.join(format!("bytevideo-{}-{serial}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("cannot create audio workspace"))
    }
}
impl Drop for TempAudio {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

#[derive(Default)]
struct Decoder {
    metadata: Option<Metadata>,
    packets: BTreeMap<u32, Vec<u8>>,
    counts: BTreeMap<u32, usize>,
    ready: usize,
    recovered: Option<Vec<u8>>,
}
impl Decoder {
    fn accept(&mut self, packet: &[u8]) -> bool {
        let Some((index, meta, payload)) = parse_packet(packet) else {
            return false;
        };
        if self.metadata.is_none() {
            self.metadata = Some(meta.clone());
        }
        if self.metadata.as_ref() != Some(&meta) || self.packets.contains_key(&index) {
            return false;
        }
        self.packets.insert(index, payload);
        let count = self.counts.entry(index / SHARDS as u32).or_default();
        *count += 1;
        if *count == DATA_SHARDS {
            self.ready += 1;
        }
        if self.ready == meta.groups as usize {
            self.recovered = recover(&meta, &self.packets).ok();
        }
        self.recovered.is_some()
    }
}

fn decode_audio(input: &str, decoder: &mut Decoder, compact: bool) -> io::Result<bool> {
    let mut child = Command::new("ffmpeg")
        .args([
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
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut assembler = repair::Assembler::default();
    let mut recognised = false;
    let reader = progress::AudioReader::new(io::BufReader::new(child.stdout.take().unwrap()));
    let result = audio::read_packets_until(
        reader,
        if compact { repair::BYTES } else { PACKET_BYTES },
        |packet| {
            if compact {
                if let Some(packet) = assembler.accept(packet) {
                    recognised = true;
                    return decoder.accept(&packet);
                }
                false
            } else {
                decoder.accept(packet)
            }
        },
    );
    let complete = decoder.recovered.is_some();
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

fn timeline(groups: usize, full_audio: bool) -> io::Result<Timeline> {
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
        groups as u64 * SHARDS as u64 * COPIES as u64 / PACKETS_PER_FRAME as u64
    };
    // RATE is divisible by FPS: pad audio by less than one video frame.
    Ok(Timeline {
        frames,
        samples: frames * (audio::RATE / FPS) as u64,
    })
}

fn encode(input: &str, output: &str, full_audio: bool) -> io::Result<()> {
    let data = fs::read(input)?;
    if data.len().div_ceil(DATA_BYTES * DATA_SHARDS).max(1) > (u32::MAX as usize / SHARDS) {
        return Err(io::Error::other("input too large"));
    }
    let mut preparation = Progress::new("Preparing data", None);
    let (meta, groups) = make_shards(&data);
    preparation.finish();
    let timeline = timeline(groups.len(), full_audio)?;
    let input_rate = if full_audio {
        format!(
            "{}/{}",
            COPIES * audio::RATE,
            PACKETS_PER_FRAME * audio::packet_samples(PACKET_BYTES)
        )
    } else {
        FPS.to_string()
    };
    let video_filter = format!(
        "fps={FPS}:round=up,tpad=stop_mode=clone:stop_duration=1,trim=end_frame={}",
        timeline.frames
    );
    let temp = TempAudio::new()?;
    let audio_path = temp.0.join("audio.f32le");
    {
        let mut writer = io::BufWriter::new(fs::File::create(&audio_path)?);
        let mut written = 0u64;
        let mut progress = Progress::new("Preparing audio", Some(timeline.samples));
        if full_audio {
            for shard in 0..SHARDS {
                for (group, shards) in groups.iter().enumerate() {
                    audio::write_packet(
                        &mut writer,
                        &make_packet((group * SHARDS + shard) as u32, &meta, &shards[shard]),
                    )?;
                    written += audio::packet_samples(PACKET_BYTES) as u64;
                    progress.update(written, Some(timeline.samples));
                }
            }
        } else {
            // Spend a fixed audio budget on parity shards first. Complete fragments
            // only: no truncated FSK symbols or packets at the end of the video.
            let mut budget = timeline.samples / audio::packet_samples(repair::BYTES) as u64;
            'packets: for shard in (DATA_SHARDS..SHARDS).chain(0..DATA_SHARDS) {
                for (group, shards) in groups.iter().enumerate() {
                    let index = (group * SHARDS + shard) as u32;
                    let parts =
                        repair::fragments(index, &make_packet(index, &meta, &shards[shard]));
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
    let mut child = Command::new("ffmpeg")
        .args([
            "-y",
            "-nostats",
            "-progress",
            "pipe:1",
            "-v",
            "error",
            "-f",
            "rawvideo",
            "-pixel_format",
            "gray",
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
            "16",
            "-pix_fmt",
            "yuv420p",
            // MP4's default 1 kHz movie clock rounds AAC edit durations to
            // milliseconds. Use the audio clock so both tracks end sample-exact.
            "-movie_timescale",
            &audio::RATE.to_string(),
            "-movflags",
            "+faststart",
            output,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let output = child.stdout.take().unwrap();
    let total_frames = timeline.frames;
    let progress_thread = std::thread::spawn(move || {
        let mut progress = Progress::new("Encoding video ", Some(total_frames));
        for line in io::BufReader::new(output).lines() {
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
        let mut pending = Vec::with_capacity(PACKET_BYTES * PACKETS_PER_FRAME);
        for _ in 0..COPIES {
            for shard in 0..SHARDS {
                for (group, shards) in groups.iter().enumerate() {
                    let packet =
                        make_packet((group * SHARDS + shard) as u32, &meta, &shards[shard]);
                    pending.extend_from_slice(&packet);
                    if pending.len() == PACKET_BYTES * PACKETS_PER_FRAME {
                        pipe.write_all(&packets_to_frame(&pending))?;
                        pending.clear();
                    }
                }
            }
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
    progress.finish();
    println!(
        "Encoded {} bytes into {} frames ({:.3} seconds, audio and video)",
        data.len(),
        timeline.frames,
        timeline.samples as f64 / audio::RATE as f64
    );
    Ok(())
}

fn decode(input: &str, output: &str) -> io::Result<()> {
    let mut child = Command::new("ffmpeg")
        .args([
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
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut reader = child.stdout.take().unwrap();
    let mut decoder = Decoder::default();
    let mut frames = 0usize;
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
            // CRCs identify the format and reject damaged packets independently.
            for four_gray in [true, false] {
                for packet in frame_to_packets(&frame, four_gray)
                    .as_chunks::<PACKET_BYTES>()
                    .0
                {
                    if decoder.accept(packet) {
                        break;
                    }
                }
                if decoder.recovered.is_some() {
                    break;
                }
            }
            let useful: usize = decoder.counts.values().map(|&n| n.min(DATA_SHARDS)).sum();
            let total = decoder
                .metadata
                .as_ref()
                .map(|m| m.groups as u64 * DATA_SHARDS as u64);
            progress.update(useful as u64, total);
            if decoder.recovered.is_some() {
                break;
            }
        }
        Ok(())
    })();
    if decoder.recovered.is_some() {
        progress.finish();
    }
    drop(progress);
    let complete = decoder.recovered.is_some();
    // A verified early stop deliberately closes the pipe. Reap ffmpeg and do not
    // report its expected SIGPIPE/termination as a decoding error.
    if video_result.is_err() || complete {
        let _ = child.kill();
    }
    drop(reader);
    let status = child.wait()?;
    let mut audio_used = false;
    if !complete {
        audio_used = true;
        if !status.success() || video_result.is_err() {
            eprintln!("Video incomplete; trying audio repair");
        }
        let compact = decode_audio(input, &mut decoder, true).unwrap_or(false);
        // Old BV02 files and --full-audio use the original full-packet modem.
        if !compact && decoder.recovered.is_none() {
            let _ = decode_audio(input, &mut decoder, false);
        }
    }
    let data = match decoder.recovered {
        Some(data) => data,
        None => {
            let meta = decoder
                .metadata
                .ok_or_else(|| io::Error::other("no valid BV02 packets found"))?;
            recover(&meta, &decoder.packets)?
        }
    };
    fs::write(output, &data)?;
    println!(
        "Recovered {} bytes into {} ({} video frames read; audio {})",
        data.len(),
        output,
        frames,
        if audio_used { "used" } else { "skipped" }
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 && !(args.len() == 5 && args[1] == "encode" && args[4] == "--full-audio") {
        eprintln!("Usage: bytevideo encode input.bin output.mp4 [--full-audio]\n       bytevideo decode input.mp4 output.bin");
        std::process::exit(2);
    }
    match args[1].as_str() {
        "encode" => encode(&args[2], &args[3], args.len() == 5),
        "decode" => decode(&args[2], &args[3]),
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
        let mut decoder = Decoder::default();
        for (i, payload) in groups[0].iter().enumerate().take(DATA_SHARDS - 1) {
            assert!(!decoder.accept(&make_packet(i as u32, &meta, payload)));
        }
        assert!(decoder.accept(&make_packet(15, &meta, &groups[0][15])));
        assert_eq!(decoder.recovered.unwrap(), b"verified");
        meta.hash[0] ^= 1;
        let mut decoder = Decoder::default();
        for (i, payload) in groups[0].iter().enumerate() {
            assert!(!decoder.accept(&make_packet(i as u32, &meta, payload)));
        }
        assert!(decoder.recovered.is_none());
    }

    #[test]
    fn empty_file_roundtrip() {
        let (meta, packets) = packets(b"");
        assert_eq!(recover(&meta, &packets).unwrap(), b"");
    }

    #[test]
    fn four_gray_levels_and_brightness_tolerance() {
        // 00, 01, 10, 11 in every byte exercises all four levels equally.
        let packed = vec![0b00011011; PACKET_BYTES * PACKETS_PER_FRAME];
        let frame = packets_to_frame(&packed);
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
        let mut frame = packets_to_frame(&packed);
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
