# Streaming memory measurements

Measured on 2026-11-14 with release builds and `/usr/bin/time`, using a fresh test process for every stage and input size. RSS is reported in KiB (1024 bytes).

| Stage | Input | Before streaming, peak RSS | Streaming, peak RSS |
| --- | ---: | ---: | ---: |
| Encode core | 8 MiB | 23,916 KiB | 3,500 KiB |
| Encode core | 64 MiB | 168,868 KiB | 3,464 KiB |
| Encode core | 256 MiB | Not measured | 3,488 KiB |
| Decode core | 8 MiB | 20,064 KiB | 3,504 KiB |
| Decode core | 64 MiB | 136,464 KiB | 3,588 KiB |
| Decode core | 256 MiB | Not measured | 3,628 KiB |

The baseline used a snapshot of the implementation before this change. Encoding allocated the input and all data/parity shards. Decoding received the first 16 data-shard positions interleaved across all groups, then reconstructed and verified the complete result in memory.

The streaming probe creates input on disk with a bounded buffer and prepares the on-disk shard spool. Its decode stage receives the same interleaved shard positions through the production decoder, publishes the verified result and checks its size and SHA-256. Every input byte is 37; the content is fixed so runs are reproducible. Production buffers have the same bounds for random bytes. The ordinary integration suite uses deterministic random input and exercises parity reconstruction, compression and audio repair.

These measurements cover the core data path, including the Rust test harness. They exclude FFmpeg, video/audio transcoding, and OS filesystem cache. They demonstrate that payload memory stays roughly constant; they do not measure total machine memory, end-to-end throughput or MP4 size. Disk-backed staging adds I/O and temporary disk usage.

Reproduce the streaming measurements on Linux:

```sh
python3 tests/memory.py
# Smaller selection:
python3 tests/memory.py --sizes 8 64
```

The script compiles the release test executable, then runs the ignored `streaming::tests::memory_profile` test in separate processes under `/usr/bin/time`. The probe needs temporary disk space for the input, shard spool and recovered output; ordinary `cargo test` skips it. Small RSS differences between runs are expected.
