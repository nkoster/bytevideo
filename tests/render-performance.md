# Frame renderer optimization

Measured on 2026-11-16 with release builds. These are local measurements, not guaranteed speedups on other machines or inputs.

The renderer precomputes the row patterns for all 256 byte values and, in color mode, all three starting hues. It also precomputes frame offsets. A single output frame buffer is allocated for an encode and overwritten in place. There are no frame allocations or color calculations in the render loop.

A regression test compares the entire rendered buffer with the previous implementation for both modes, every byte value and every starting hue across multiple frames. It also checks that the buffer address and capacity do not change.

## Rendering only

Median of three runs, 5,000 frames per run; excludes FFmpeg, packet preparation and I/O. Both implementations receive the same deterministic data. The reference implementation is compiled only into the test executable.

| Mode | Original | Precomputed/reused | Less rendering time |
| --- | ---: | ---: | ---: |
| Grayscale | 0.2360 s | 0.1287 s | 45.5% |
| Color | 0.5277 s | 0.2374 s | 55.0% |

```sh
python3 tests/render_benchmark.py
```

## Complete encode

A separate comparison uses 2 MiB of deterministic random input, the default encoding settings and a saved release binary from before the optimization. Old/new run order alternates. Three runs are timed per mode and implementation. Every recording is decoded and compared with the input outside the timed interval.

The complete encode also includes Reed-Solomon preparation, temporary disk I/O, FFmpeg and output publication. Small differences here can be affected by system load and timing noise; the rendering-only gain is not the complete encode gain.

| Mode | Original median | Optimized median | Less total time |
| --- | ---: | ---: | ---: |
| Grayscale | 10.7414 s | 10.1595 s | 5.4% |
| Color | 11.7863 s | 11.6717 s | 1.0% |

Raw encode times in seconds:

- Grayscale original: 9.7704, 10.7414, 11.4829; optimized: 9.8131, 10.1595, 11.4307.
- Color original: 11.7986, 11.7080, 11.7863; optimized: 11.5811, 11.6883, 11.6717.

The color result shows little end-to-end improvement despite faster frame generation. The grayscale runs varied substantially, so their median difference should also be treated cautiously.

To reproduce with a saved older binary:

```sh
python3 tests/render_benchmark.py --baseline-bin /path/to/previous/bytevideo
```
