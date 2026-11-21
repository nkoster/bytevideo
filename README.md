# bytevideo

bytevideo stores files in a video and recovers the original bytes from that recording. It turns data into a mosaic of brightness tiles in an H.264 MP4, with additional recovery data in the audio track. Any file can be used as input; its contents are encoded as data.

By default, it uses ten grayscale levels and 8×8-pixel tiles. You can experiment with fewer brightness levels, smaller tiles, decorative colors, or a complete audio backup. Use `--gray-levels 2` for the original two-level encoding. Recovery is checked against the original file's SHA-256 before the output is written.

Observed YouTube results with ten grayscale levels: YouTube's compression reduced an MP4 from 799 MB to approximately 207 MB. The downloaded version decoded successfully, recovering the original data intact despite the recompression.

## Usage

### Requirements and build

Install Rust and FFmpeg with the libx264 encoder and AAC support. The integration tests also require `ffprobe` and Python 3.

FFmpeg 4.4 is supported. When FFmpeg provides `-movie_timescale`, bytevideo uses it for sample-exact MP4 track durations; older versions use the default movie clock, which can round audio edit durations to milliseconds.

```sh
cargo build --release
```

The examples below use `./target/release/bytevideo`. For a debug build, run `cargo build` and use `./target/debug/bytevideo` instead.

### Encode and decode

```sh
./target/release/bytevideo encode input.bin output.mp4
./target/release/bytevideo decode output.mp4 recovered.bin
cmp input.bin recovered.bin
```

For example, to store and recover an MP3:

```sh
./target/release/bytevideo encode original.mp3 recording.mp4
./target/release/bytevideo decode recording.mp4 recovered.mp3
cmp original.mp3 recovered.mp3
```

The decoder automatically detects the tile size and brightness levels. It reads video first and tries audio repair if needed. Both commands replace an existing destination on success and preserve it on failure.

### Encode options

Place options after the output filename. They can be combined in any order. Value options also accept `--name=value`.

| Option | Values | Default | Effect |
| --- | --- | --- | --- |
| `--tile-size N` | 2–10 | 8 | Sets the width and height of each square tile in pixels. |
| `--gray-levels N` | 2–10 | 10 | Sets the number of brightness levels used for data. |
| `--color [NAME]` | `red`, `green`, `blue`, `rasta` | Off | Uses one tint, or the Rasta pattern when no name is given. Also accepts `--color=NAME`. |
| `--full-audio` | Flag | Off | Adds a complete audio backup and extends the recording to fit it. |

```sh
# Default ten levels, with red/yellow/green tiles.
./target/release/bytevideo encode input.bin colorful.mp4 --color

# Four brightness levels, with red/yellow/green tiles.
./target/release/bytevideo encode input.bin four-levels.mp4 --gray-levels 4 --color

# Ten brightness levels, with only red tints.
./target/release/bytevideo encode input.bin red.mp4 --gray-levels 10 --color red

# Smaller tiles for roundtrip experiments.
./target/release/bytevideo encode input.bin tiles3.mp4 --tile-size 3

# A complete independent backup in audio; substantially slower.
./target/release/bytevideo encode input.bin backup.mp4 --full-audio

# Options can be combined.
./target/release/bytevideo encode input.bin experiment.mp4 --tile-size 6 --gray-levels 3 --color
```

Smaller tiles and closer brightness levels can be more vulnerable to compression and scaling. More capacity does not necessarily mean a smaller MP4. At 5–10 levels, the encoder increases quality to preserve the smaller brightness differences, which can increase file size. Color is decorative and does not add data capacity.

### Progress and roundtrip testing

Progress is displayed on stderr when connected to a terminal. Encoding shows audio preparation and actual FFmpeg output frames. Video decoding counts useful shards recovered toward reconstruction, so it can reach 100% before the recording ends. The percentage is a packet-recovery target, not the fraction of the file already restored: every group must have enough valid shards. Audio scanning uses a spinner when the total is unknown. Redirected output contains no progress display.

For a YouTube roundtrip, test a processed YouTube rendition rather than the original upload. Check that the downloaded file contains the intended video resolution and an audio stream if audio repair is needed. Recovery after YouTube processing or other transcoding is not guaranteed.

## Technical details

### Video format and error recovery

With the default settings, each frame has 5,929 bytes of transport capacity, filled with BV02 packets and fragments. Each ordinary BV02 packet has a 64-byte header and a 1,736-byte shard. Tiles are 8×8 pixels; decoding averages their central 4×4 pixels. Each group contains 16 data shards and 8 Reed-Solomon parity shards. Any 16 valid distinct shards in a group suffice for reconstruction. Three interleaved passes separate packet copies in time. Packet CRC32 detects damage; the original file's SHA-256 must match before output is written.

Video determines the duration: with the default ten levels and 8×8 tiles, one group of up to 27,776 input bytes takes 23 frames (0.767 seconds); 100,000 bytes takes 91 frames (3.033 seconds). Exact duration depends on packet and fragment placement. Two levels take 2.4 seconds per group; four levels take 1.2 seconds. Both tracks start at zero and end together. H.264 uses CRF 16 at 2–4 levels (higher quality at 5–10 levels), the `veryfast` preset, 1280×720 resolution and constant 30 fps.

### Brightness encoding

Brightness levels are evenly spaced from 16 to 235. The default uses ten levels. Two levels use 16 and 235; four levels use 16, 89, 162 and 235.

Two and four levels retain their exact bit mappings: eight or four tiles per byte. Other level counts automatically select the most efficient radix block of one to seven bytes, preferring smaller blocks when efficiency is equal. Blocks can cross tile rows. For example, ten levels encode seven bytes in seventeen tiles instead of twenty-one; eight levels encode three bytes in eight tiles. This uses more of the available brightness combinations. For 5–10 levels, the encoder reduces CRF by two per added level (14 down to 4) to preserve the closer brightness values in the generated MP4. This can increase file size. Closer brightness levels are more vulnerable to recompression; local recovery does not guarantee a YouTube roundtrip.

### Tile layout and compression

BV02 packets are 1,800 bytes, regardless of tile size or brightness level count. At three and five through ten levels, the encoder fills each frame with whole packets and splits a packet into eight CRC-protected 256-byte fragments when it does not fit in the remaining space. Fragments continue in the next frame and support frame loss, duplication and reordering. Unused transport space is less than 256 bytes per non-final frame; the final frame is padded. A few remaining tiles and edge pixels can also be unused. Two and four levels retain whole-byte rows and whole-packet packing. Odd sizes work with YUV420p too. Smaller tiles increase capacity and shorten fast-mode videos, but may be more vulnerable to compression and scaling. YouTube roundtrip reliability needs testing for each size.

At 8×8, two levels carry one packet per frame and four levels carry two. At 4×4, two levels fit four packets and four levels fit eight. For these level counts, fast-mode duration is `ceil(groups × 24 × 3 / packets_per_frame) / 30` seconds. Two levels at 9×9/10×10 use eight 256-byte fragments per packet instead. Other level counts calculate the exact frame count from their radix capacity and mixed packet/fragment placement. For three groups, ten levels at 8×8 now take 69 frames instead of 108. Full-audio duration stays the same at every tile size and level count.

The decoder automatically detects tile size and brightness levels using packet/fragment CRCs. Recordings made with the previous layout at three or five through ten levels must be re-encoded. Two- and four-level recordings retain their existing layout.

MP4 size does **not** decrease steadily with tile size. The encoder uses a quality target rather than a fixed bitrate (CRF 16 at 2–4 levels). Tile edges at multiples of four align with H.264's transform blocks; sizes such as 5 can need a much higher bitrate.

### Color rendering

`--color` adds a fixed red/green pattern for two levels, or a red/yellow/green Rasta pattern for 3–10 levels. `--color red`, `--color green` and `--color blue` use the same coloring process with only the selected hue. `--color rasta` explicitly selects the multicolor pattern. Each hue has the selected number of brightness variants; saturation is lower near black and white. Data uses luma, mapped to limited-range YUV to match ordinary gray-input conversion. Output remains H.264 YUV420p. Packet layout, capacity, timing and redundancy are the same with and without color for the same tile size and gray-level count.

At two and four levels, the renderer precomputes tile-row patterns for all data bytes and starting hues. Other level counts render radix blocks across the tile grid. Both reuse one frame buffer throughout encoding. Run `python3 tests/render_benchmark.py` to measure rendering speed without FFmpeg; this does not measure the complete encode speed.

Chroma adds encoding work and can increase MP4 size. Tests cover video-only recovery after H.264 CRF 23/28 recompression and loss/audio-repair cases in both appearances.

### Audio repair

Audio supplies additional copies of selected shards within the video duration. The transport uses 2/4 kHz binary FSK at 2,000 bits/s, 48 kHz sampling, and mono AAC at 192 kbit/s. CRC-protected BVA1 fragments contain 238 bytes of packet data each. Deterministic trailing zero padding is omitted, so tiny files can also receive useful audio repair. Fragments are reassembled and checked against the embedded BV02 packet CRC before entering the ordinary recovery process.

The encoder prioritizes parity shards and only sends packets whose complete fragment set fits in the available audio budget. A fragment lasts 1.056 seconds; a full-size shard requires eight fragments. With the default ten levels, a single full-size shard fits starting at twelve groups (9.1 seconds). Tiny, mostly padded packets often fit in a single fragment. If no complete repair packet fits, the audio track is silence. Audio is supplemental: fast mode does not promise independent audio-only recovery or an additional parity allowance in every group. Surviving audio and video copies share the same 16-of-24 reconstruction threshold.

### Full audio backup and timing

`--full-audio` writes a complete audio pass of all data and parity shards. Video packets are held and distributed throughout the entire audio duration. One group takes 173.600 seconds; either channel can recover the file independently when enough valid shards survive. The decoder stops early when video alone suffices.

Timing uses integer audio sample and video frame counts. Full mode pads audio to a 30 fps frame boundary; when supported by FFmpeg, the MP4 movie clock runs at 48 kHz to avoid millisecond rounding of AAC edit durations. Fast mode calculates the frame count from the selected brightness levels, tile size and packet placement, then pads unused audio time with silence. Audio and video are aligned to the same end time, subject to millisecond rounding on older FFmpeg versions.

### Decoding and supported formats

The decoder tries video first. It tracks valid distinct shards per group and attempts reconstruction as soon as every group has at least 16. After SHA-256 verification, it terminates and reaps ffmpeg, skipping the rest of the video and all audio. If video is insufficient, it reads compact audio repair; it also supports the full-packet audio transport used by `--full-audio`. Expected early termination of ffmpeg is not reported as an error.

The decoder supports BV02 recordings with 2–10 brightness levels, compact audio repair, and full-packet audio. BV01 is unsupported.

### Memory, temporary files and output safety

Input is hashed in a first pass, then encoded one Reed-Solomon group at a time into a temporary shard file. Video interleaving and audio repair read that file in playback order. A second hash detects changes to the input between passes. Decoding stores shards, group status and incomplete audio fragments on disk; completed groups are reconstructed directly into a temporary output file. Core payload buffers remain bounded as the file grows. See [streaming memory measurements](tests/streaming-memory.md) for measurements excluding FFmpeg.

Encoding also uses a temporary raw audio file. Shard and audio staging use the system temporary directory (`TMPDIR` on Unix). Decode staging and the encoded MP4 are created beside the destination so publication uses a same-filesystem rename. Decoding verifies size and SHA-256 before publication; both commands preserve an existing destination on failure and replace it on success. Temporary workspaces are cleaned up on success or a returned error. Streaming trades RAM for temporary disk space: encoder shard storage is about 1.5 times the input size, plus raw audio and the MP4; decode needs shard storage, recovered data and, when used, fragment storage. Full mode requires substantially more time and temporary disk space than fast mode. Audio is audible modem-like tones when it contains repair packets.

### Recovery limits

Shorter recordings give each packet less screen time than full-audio mode. YouTube reliability must be measured for each setting and rendition; local roundtrip tests do not establish that reliability.

Arbitrary compression, rescaling, cropping, overlays, or packet loss can exceed the available recovery budget. Synchronization handles frame loss, duplication and reordering, but does not correct cropping or rotation. Time stretching, pitch changes and aggressive audio filtering can prevent modem decoding. Container damage that prevents ffmpeg from decoding is outside the erasure layer. SHA-256 checks integrity, not authenticity.

### Validation

```sh
cargo test
cargo clippy --all-targets -- -D warnings
bash tests/roundtrip.sh
bash tests/roundtrip.sh --color
python3 tests/color.py
python3 tests/tile_size.py
bash tests/full_audio.sh
bash tests/full_audio.sh --color
# Unix terminal echo regression (encode, decode, audio repair and failure).
python3 tests/terminal.py
# Optional: core RSS benchmark, without FFmpeg (uses /usr/bin/time).
python3 tests/memory.py
```

The fast integration suite explicitly uses `--gray-levels 4` and 200,000 deterministic random bytes spanning eight groups. It checks equal track durations, 30 fps, 288 output frames, early stopping after 64 decoded frames with audio skipped, H.264 CRF 28 recompression, recovery after eight missing shards per group, failure without output when video is insufficient, and successful repair using recompressed AAC audio. It also checks recovery after a burst of black frames and compact audio repair of a damaged four-byte recording. The separate full-audio test checks equal track durations and audio-only recovery after AAC recompression.

Unit tests cover every level count and tile size, binary red/green coloring, brightness shifts, two-level tiles, reconstruction, excessive loss, hash mismatch, tile noise, packet and fragment CRCs, fragment ordering and padding, audio synchronization and AAC, early-stop verification, and timelines in both modes.
