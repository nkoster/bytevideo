//! Disk-backed staging keeps payloads and per-group state out of RAM.
use crate::progress::Progress;
use crate::{codec, make_packet, parse_packet, Metadata, DATA_BYTES, DATA_SHARDS, SHARDS};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const GROUP_BYTES: usize = DATA_BYTES * DATA_SHARDS;
const COMPLETE: u32 = 1 << 31;

pub struct Workspace(pub PathBuf);
impl Workspace {
    pub fn new() -> io::Result<Self> {
        Self::in_dir(&env::temp_dir())
    }

    pub fn beside(output: &Path) -> io::Result<Self> {
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Self::in_dir(parent)
    }

    pub fn in_dir(base: &Path) -> io::Result<Self> {
        for serial in 0..1000 {
            let path = base.join(format!(".bytevideo-{}-{serial}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("cannot create temporary workspace"))
    }

    pub fn file(&self, name: &str) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.0.join(name))
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn hash_reader(mut reader: impl Read) -> io::Result<(u64, [u8; 32])> {
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size = size
            .checked_add(n as u64)
            .ok_or_else(|| io::Error::other("input too large"))?;
    }
    Ok((size, hash.finalize().into()))
}

pub struct PreparedInput {
    pub metadata: Metadata,
    shards: File,
    // Drop open files before deleting their directory (also on Windows).
    _workspace: Workspace,
}
impl PreparedInput {
    pub fn new(input: &Path) -> io::Result<Self> {
        let mut source = File::open(input)?;
        let mut progress = Progress::new("Hashing input", None);
        let (size, hash) = hash_reader(&mut source)?;
        progress.finish();
        let groups = size.div_ceil(GROUP_BYTES as u64).max(1);
        if groups > (u32::MAX / SHARDS as u32) as u64 {
            return Err(io::Error::other("input too large"));
        }
        let metadata = Metadata {
            groups: groups as u32,
            size,
            hash,
        };
        source.rewind()?;
        Self::spool(source, metadata)
    }

    fn spool(mut source: impl Read, metadata: Metadata) -> io::Result<Self> {
        let Metadata { groups, size, hash } = metadata.clone();
        let groups = groups as u64;
        let workspace = Workspace::new()?;
        let mut shards_file = workspace.file("shards")?;
        let mut writer = io::BufWriter::new(&mut shards_file);
        let mut progress = Progress::new("Preparing data ", Some(groups));
        let rs = codec();
        let mut second_hash = Sha256::new();
        let mut remaining = size;
        for group in 0..groups {
            let mut shards = vec![vec![0u8; DATA_BYTES]; SHARDS];
            for shard in shards.iter_mut().take(DATA_SHARDS) {
                let n = remaining.min(DATA_BYTES as u64) as usize;
                source.read_exact(&mut shard[..n])?;
                second_hash.update(&shard[..n]);
                remaining -= n as u64;
            }
            rs.encode(&mut shards).map_err(io::Error::other)?;
            for shard in &shards {
                writer.write_all(shard)?;
            }
            progress.update(group + 1, Some(groups));
        }
        let mut extra = [0u8; 1];
        if source.read(&mut extra)? != 0 || <[u8; 32]>::from(second_hash.finalize()) != hash {
            return Err(io::Error::other(
                "input changed while preparing shards; output not written",
            ));
        }
        writer.flush()?;
        drop(writer);
        progress.finish();
        Ok(Self {
            metadata,
            shards: shards_file,
            _workspace: workspace,
        })
    }

    // Group-major disk storage supports the original shard-major playback order.
    pub fn packet(&mut self, group: u32, shard: usize) -> io::Result<Vec<u8>> {
        let index = group as u64 * SHARDS as u64 + shard as u64;
        self.shards
            .seek(SeekFrom::Start(index * DATA_BYTES as u64))?;
        let mut payload = vec![0u8; DATA_BYTES];
        self.shards.read_exact(&mut payload)?;
        Ok(make_packet(index as u32, &self.metadata, &payload))
    }
}

pub struct Decoder {
    pub metadata: Option<Metadata>,
    pub useful: u64,
    pub verified: bool,
    error: Option<io::Error>,
    ready: u32,
    shards: File,
    states: File,
    output: File,
    workspace: Workspace,
}
impl Decoder {
    pub fn new(output: &Path) -> io::Result<Self> {
        // Staging beside the destination makes publication a same-filesystem rename.
        let workspace = Workspace::beside(output)?;
        Ok(Self {
            metadata: None,
            useful: 0,
            verified: false,
            error: None,
            ready: 0,
            shards: workspace.file("shards")?,
            states: workspace.file("states")?,
            output: workspace.file("output")?,
            workspace,
        })
    }

    pub fn stopped(&self) -> bool {
        self.verified || self.error.is_some()
    }

    pub fn fail(&mut self, error: io::Error) {
        self.error = Some(error);
    }

    pub fn check_error(&mut self) -> io::Result<()> {
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    // The modem callback uses bool for early termination. Preserve I/O errors
    // separately so disk failures cannot be mistaken for damaged media.
    pub fn accept(&mut self, packet: &[u8]) -> bool {
        if self.stopped() {
            return true;
        }
        if let Err(e) = self.accept_inner(packet) {
            self.error = Some(e);
        }
        self.stopped()
    }

    fn state(&mut self, group: u32) -> io::Result<u32> {
        self.states.seek(SeekFrom::Start(group as u64 * 4))?;
        let mut bytes = [0u8; 4];
        let n = self.states.read(&mut bytes)?;
        if n == 0 {
            return Ok(0);
        }
        self.states.read_exact(&mut bytes[n..])?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn accept_inner(&mut self, packet: &[u8]) -> io::Result<()> {
        let Some((index, meta, payload)) = parse_packet(packet) else {
            return Ok(());
        };
        if self.metadata.is_none() {
            self.metadata = Some(meta.clone());
        }
        if self.metadata.as_ref() != Some(&meta) {
            return Ok(());
        }
        let group = index / SHARDS as u32;
        let bit = 1 << (index % SHARDS as u32);
        let mut mask = self.state(group)?;
        if mask & (COMPLETE | bit) != 0 {
            return Ok(());
        }
        self.shards
            .seek(SeekFrom::Start(index as u64 * DATA_BYTES as u64))?;
        self.shards.write_all(&payload)?;
        mask |= bit;
        self.useful += 1;
        if mask.count_ones() == DATA_SHARDS as u32 {
            self.reconstruct(group, mask, &meta)?;
            mask |= COMPLETE;
            self.ready += 1;
        }
        self.states.seek(SeekFrom::Start(group as u64 * 4))?;
        self.states.write_all(&mask.to_le_bytes())?;
        if self.ready == meta.groups {
            self.verify()?;
        }
        Ok(())
    }

    fn reconstruct(&mut self, group: u32, mask: u32, meta: &Metadata) -> io::Result<()> {
        let mut shards = vec![None; SHARDS];
        for (i, shard) in shards.iter_mut().enumerate() {
            if mask & (1 << i) != 0 {
                let offset = (group as u64 * SHARDS as u64 + i as u64) * DATA_BYTES as u64;
                self.shards.seek(SeekFrom::Start(offset))?;
                let mut payload = vec![0u8; DATA_BYTES];
                self.shards.read_exact(&mut payload)?;
                *shard = Some(payload);
            }
        }
        codec()
            .reconstruct_data(&mut shards)
            .map_err(io::Error::other)?;
        let offset = group as u64 * GROUP_BYTES as u64;
        self.output.seek(SeekFrom::Start(offset))?;
        let mut remaining = meta.size.saturating_sub(offset).min(GROUP_BYTES as u64) as usize;
        for shard in shards.into_iter().take(DATA_SHARDS) {
            let n = remaining.min(DATA_BYTES);
            self.output.write_all(&shard.unwrap()[..n])?;
            remaining -= n;
        }
        Ok(())
    }

    fn verify(&mut self) -> io::Result<()> {
        let meta = self
            .metadata
            .as_ref()
            .ok_or_else(|| io::Error::other("no valid BV02 packets found"))?;
        if self.ready != meta.groups {
            return Err(io::Error::other(format!(
                "cannot recover file: {}/{} groups complete; output not written",
                self.ready, meta.groups
            )));
        }
        self.output.rewind()?;
        let (size, hash) = hash_reader(&mut self.output)?;
        if size != meta.size || hash != meta.hash {
            return Err(io::Error::other(
                "file SHA-256 mismatch; output not written",
            ));
        }
        self.verified = true;
        Ok(())
    }

    pub fn finish(mut self, destination: &Path) -> io::Result<u64> {
        self.check_error()?;
        if !self.verified {
            self.verify()?;
        }
        self.output.sync_all()?;
        let size = self.metadata.as_ref().unwrap().size;
        fs::rename(self.workspace.0.join("output"), destination)?;
        Ok(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{make_shards, PARITY_SHARDS};
    use std::io::Cursor;

    fn prepare(work: &Workspace, data: &[u8]) -> PreparedInput {
        let input = work.0.join("input");
        fs::write(&input, data).unwrap();
        PreparedInput::new(&input).unwrap()
    }

    #[test]
    fn spooled_packets_match_existing_format_at_group_boundaries() {
        let work = Workspace::new().unwrap();
        for size in [
            0,
            1,
            DATA_BYTES,
            GROUP_BYTES - 1,
            GROUP_BYTES,
            GROUP_BYTES + 1,
            2 * GROUP_BYTES + 97,
        ] {
            let data: Vec<u8> = (0..size).map(|i| (i * 71 + 3) as u8).collect();
            let (meta, groups) = make_shards(&data);
            let mut prepared = prepare(&work, &data);
            assert_eq!(prepared.metadata, meta);
            for shard in (0..SHARDS).rev() {
                for (group, payloads) in groups.iter().enumerate().rev() {
                    assert_eq!(
                        prepared.packet(group as u32, shard).unwrap(),
                        make_packet((group * SHARDS + shard) as u32, &meta, &payloads[shard])
                    );
                }
            }
        }
    }

    #[test]
    fn reconstructs_reordered_groups_with_loss_duplicates_and_padding() {
        let work = Workspace::new().unwrap();
        let output = work.0.join("restored");
        let data: Vec<u8> = (0..GROUP_BYTES * 2 + 317).map(|i| (i * 13) as u8).collect();
        let mut prepared = prepare(&work, &data);
        let mut decoder = Decoder::new(&output).unwrap();
        let staging = decoder.workspace.0.clone();
        // Interleave all three groups, receive the last group first, and lose
        // eight shards (including data shards) in every group.
        for shard in (PARITY_SHARDS..SHARDS).rev() {
            for group in (0..prepared.metadata.groups).rev() {
                let packet = prepared.packet(group, shard).unwrap();
                decoder.accept(&packet);
                let useful = decoder.useful;
                decoder.accept(&packet);
                assert_eq!(decoder.useful, useful);
                assert!(!output.exists());
            }
        }
        assert!(decoder.verified);
        assert_eq!(decoder.finish(&output).unwrap(), data.len() as u64);
        assert_eq!(fs::read(&output).unwrap(), data);
        assert!(!staging.exists());
    }

    #[test]
    fn incomplete_group_preserves_existing_output_and_cleans_staging() {
        let work = Workspace::new().unwrap();
        let output = work.0.join("restored");
        fs::write(&output, b"existing output").unwrap();
        let mut prepared = prepare(&work, b"incomplete");
        let mut decoder = Decoder::new(&output).unwrap();
        let staging = decoder.workspace.0.clone();
        for shard in 0..DATA_SHARDS - 1 {
            assert!(!decoder.accept(&prepared.packet(0, shard).unwrap()));
        }
        assert!(decoder.finish(&output).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing output");
        assert!(!staging.exists());
    }

    #[test]
    fn empty_file_and_successful_output_replacement() {
        let work = Workspace::new().unwrap();
        let output = work.0.join("restored");
        fs::write(&output, b"old longer file").unwrap();
        let mut prepared = prepare(&work, b"");
        let mut decoder = Decoder::new(&output).unwrap();
        for shard in 0..DATA_SHARDS {
            decoder.accept(&prepared.packet(0, shard).unwrap());
        }
        assert!(decoder.verified);
        assert_eq!(decoder.finish(&output).unwrap(), 0);
        assert_eq!(fs::metadata(&output).unwrap().len(), 0);
    }

    #[test]
    fn rejects_input_changed_between_passes() {
        let data = b"initial contents";
        let (metadata, _) = make_shards(data);
        for changed in [
            b"changed contents".as_slice(),
            b"short",
            b"initial contents plus more",
        ] {
            assert!(PreparedInput::spool(Cursor::new(changed), metadata.clone()).is_err());
        }
    }

    #[test]
    fn disk_errors_stop_decode_without_publishing() {
        let work = Workspace::new().unwrap();
        let output = work.0.join("restored");
        let mut prepared = prepare(&work, b"disk failure");
        let mut decoder = Decoder::new(&output).unwrap();
        let staging = decoder.workspace.0.clone();
        // A read-only shard store injects a deterministic write failure.
        decoder.shards = File::open(decoder.workspace.0.join("shards")).unwrap();
        assert!(decoder.accept(&prepared.packet(0, 0).unwrap()));
        assert!(!decoder.verified);
        assert!(decoder.finish(&output).is_err());
        assert!(!output.exists());
        assert!(!staging.exists());
    }

    #[test]
    fn failed_publication_cleans_staging() {
        let work = Workspace::new().unwrap();
        let output = work.0.join("directory");
        fs::create_dir(&output).unwrap();
        let mut prepared = prepare(&work, b"cannot replace a directory");
        let mut decoder = Decoder::new(&output).unwrap();
        let staging = decoder.workspace.0.clone();
        for shard in 0..DATA_SHARDS {
            decoder.accept(&prepared.packet(0, shard).unwrap());
        }
        assert!(decoder.verified);
        assert!(decoder.finish(&output).is_err());
        assert!(output.is_dir());
        assert!(!staging.exists());
    }

    /// Run in a fresh process under /usr/bin/time; excludes FFmpeg memory.
    /// PROFILE_MIB=64 PROFILE_STAGE=encode|decode <test-binary>
    ///   --ignored --exact streaming::tests::memory_profile
    #[test]
    #[ignore = "manual RSS benchmark; writes a configurable amount of temporary data"]
    fn memory_profile() {
        let size: u64 = env::var("PROFILE_MIB").unwrap().parse::<u64>().unwrap() * 1024 * 1024;
        let work = Workspace::new().unwrap();
        let input = work.0.join("input");
        let mut file = File::create(&input).unwrap();
        io::copy(&mut io::repeat(37).take(size), &mut file).unwrap();
        drop(file);
        let mut prepared = PreparedInput::new(&input).unwrap();
        if env::var("PROFILE_STAGE").unwrap() == "decode" {
            let output = work.0.join("restored");
            let mut decoder = Decoder::new(&output).unwrap();
            for shard in 0..DATA_SHARDS {
                for group in 0..prepared.metadata.groups {
                    decoder.accept(&prepared.packet(group, shard).unwrap());
                }
            }
            assert!(decoder.verified);
            decoder.finish(&output).unwrap();
            assert_eq!(
                hash_reader(File::open(output).unwrap()).unwrap(),
                (prepared.metadata.size, prepared.metadata.hash)
            );
        }
        std::hint::black_box(prepared);
    }
}
