//! Small terminal-only progress display; ordinary stdout remains script-friendly.
use std::io::{self, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

pub struct Progress {
    label: &'static str,
    total: Option<u64>,
    current: u64,
    started: Instant,
    last_draw: Instant,
    visible: bool,
}

impl Progress {
    pub fn new(label: &'static str, total: Option<u64>) -> Self {
        let now = Instant::now();
        let mut progress = Self {
            label,
            total,
            current: 0,
            started: now,
            last_draw: now,
            visible: io::stderr().is_terminal(),
        };
        progress.draw();
        progress
    }

    pub fn update(&mut self, current: u64, total: Option<u64>) {
        self.current = current;
        self.total = total;
        if self.visible && self.last_draw.elapsed() >= Duration::from_millis(100) {
            self.draw();
        }
    }

    fn draw(&mut self) {
        if !self.visible {
            return;
        }
        let elapsed = self.started.elapsed().as_secs();
        let mut stderr = io::stderr().lock();
        if let Some(total) = self.total.filter(|&n| n > 0) {
            let current = self.current.min(total);
            let filled = (current as u128 * 24 / total as u128) as usize;
            let percent = current as u128 * 100 / total as u128;
            let _ = write!(
                stderr,
                "\r\x1b[2K{} [{}{}] {:3}% ({}/{}) {}s",
                self.label,
                "=".repeat(filled),
                " ".repeat(24 - filled),
                percent,
                current,
                total,
                elapsed
            );
        } else {
            let spinner = ["|", "/", "-", "\\"];
            let tick = (self.started.elapsed().as_millis() / 100 % 4) as usize;
            let _ = write!(
                stderr,
                "\r\x1b[2K{} {} {}s",
                self.label, spinner[tick], elapsed
            );
        }
        let _ = stderr.flush();
        self.last_draw = Instant::now();
    }

    pub fn finish(&mut self) {
        if let Some(total) = self.total {
            self.current = total;
        }
        self.draw();
        self.close();
    }

    fn close(&mut self) {
        if self.visible {
            let _ = writeln!(io::stderr());
            self.visible = false;
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // Errors leave the last real position visible, never a false 100%.
        self.close();
    }
}

/// Keep a spinner moving while scanning audio, even without any valid packets.
pub struct AudioReader<R> {
    inner: R,
    progress: Progress,
    bytes: u64,
    next_update: u64,
}

impl<R> AudioReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            progress: Progress::new("Audio decode", None),
            bytes: 0,
            next_update: 65536,
        }
    }
}

impl<R: Read> Read for AudioReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        if self.bytes >= self.next_update {
            self.progress.update(self.bytes, None);
            self.next_update = self.bytes.saturating_add(65536);
        }
        Ok(n)
    }
}
