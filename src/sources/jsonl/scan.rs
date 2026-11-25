use super::{compressed, io, resume_rejected};
use crate::{
    AppendState, Error, Result, Session, Timestamp,
    hex::hex,
    sources::{PREFIX_CHUNK, Resume},
};
use ruzstd::decoding::{
    BlockDecodingStrategy, FrameDecoder,
    errors::{FrameDecoderError, ReadFrameHeaderError},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};

const RECENT_MS: Timestamp = 600_000;

pub(super) struct Scan {
    consumed: u64,
    lines: u64,
    malformed: u64,
    hasher: Option<Chunks>,
    recent: bool,
    torn: bool,
    recheck: bool,
}

pub struct Tally {
    pub malformed: u64,
    pub recheck: bool,
}

impl Scan {
    const fn new(recent: bool) -> Self {
        Self {
            consumed: 0,
            lines: 0,
            malformed: 0,
            hasher: None,
            recent,
            torn: false,
            recheck: false,
        }
    }

    const fn malformed(&self) -> u64 {
        self.malformed + self.torn as u64
    }

    pub(super) fn into_state(
        self,
        session: Session,
        entries: u64,
        metadata: &fs::Metadata,
        others: String,
    ) -> AppendState {
        AppendState {
            consumed: self.consumed,
            spot_checked: self.hasher.as_ref().is_some_and(|c| c.spot_checked),
            chunks: self.hasher.map(Chunks::finish).unwrap_or_default(),
            lines: self.lines,
            entries,
            malformed: self.malformed,
            dev: metadata.dev(),
            ino: metadata.ino(),
            others,
            session,
        }
    }

    pub(super) const fn tally(&self) -> Tally {
        Tally {
            malformed: self.malformed(),
            recheck: self.recheck,
        }
    }
}

#[derive(Default)]
struct Chunks {
    digests: Vec<String>,
    current: Sha256,
    filled: u64,
    spot_checked: bool,
}

impl Chunks {
    fn update(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let room = usize::try_from(PREFIX_CHUNK - self.filled).unwrap_or(usize::MAX);
            let (taken, rest) = bytes.split_at(room.min(bytes.len()));
            self.current.update(taken);
            self.filled += taken.len() as u64;
            bytes = rest;
            if self.filled == PREFIX_CHUNK {
                self.digests
                    .push(hex(std::mem::take(&mut self.current).finalize()));
                self.filled = 0;
            }
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.filled > 0 {
            self.digests.push(hex(self.current.finalize()));
        }
        self.digests
    }
}

pub(super) fn recent(metadata: &fs::Metadata, now_ms: Timestamp) -> bool {
    let modified = metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000;
    now_ms.saturating_sub(modified) < RECENT_MS
}

fn scan(
    reader: &mut dyn BufRead,
    path: &Path,
    state: &mut Scan,
    visit: &mut dyn FnMut(u64, &Value, bool) -> Result<bool>,
) -> Result<()> {
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| io(path, e))?
            == 0
        {
            break;
        }
        let line = state.lines + 1;
        let terminated = bytes.last() == Some(&b'\n');
        if terminated {
            state.lines = line;
            state.consumed += bytes.len() as u64;
            if let Some(hasher) = &mut state.hasher {
                hasher.update(&bytes);
            }
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value.is_object() => {
                if !visit(line, &value, terminated)? {
                    break;
                }
            }
            _ if terminated => state.malformed += 1,
            _ if state.recent => state.recheck = true,
            _ => state.torn = true,
        }
    }
    Ok(())
}

pub(super) fn open_regular(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io(path, e))?;
    if !metadata.is_file() {
        return Err(Error::InvalidArgument(
            "transcript must be a regular file".into(),
        ));
    }
    File::open(path).map_err(|e| io(path, e))
}

fn decoded<'a>(path: &Path, file: &'a File) -> Result<Box<dyn BufRead + 'a>> {
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(0)).map_err(|e| io(path, e))?;
    Ok(if compressed(path) {
        Box::new(BufReader::new(Frames {
            source: reader,
            decoder: FrameDecoder::new(),
            active: false,
        }))
    } else {
        Box::new(reader)
    })
}

pub(super) fn scan_file(
    path: &Path,
    file: &File,
    recent: bool,
    visit: &mut dyn FnMut(u64, &Value) -> Result<bool>,
) -> Result<Tally> {
    let mut state = Scan::new(recent && !compressed(path));
    scan(
        &mut *decoded(path, file)?,
        path,
        &mut state,
        &mut |line, record, _| visit(line, record),
    )?;
    Ok(state.tally())
}

pub fn records(
    path: &Path,
    now_ms: Timestamp,
    visit: &mut dyn FnMut(u64, &Value) -> Result<bool>,
) -> Result<Tally> {
    let file = open_regular(path)?;
    let metadata = file.metadata().map_err(|e| io(path, e))?;
    scan_file(path, &file, recent(&metadata, now_ms), visit)
}

pub(super) fn leading(path: &Path, visit: &mut dyn FnMut(&Value) -> bool) -> Result<()> {
    scan_file(path, &open_regular(path)?, false, &mut |_, record| {
        Ok(visit(record))
    })
    .map(drop)
}

pub(super) fn appendable(
    path: &Path,
    file: &File,
    recent: bool,
    resume: Option<&Resume>,
    visit: &mut dyn FnMut(u64, &Value, bool) -> Result<bool>,
) -> Result<Scan> {
    let mut reader = BufReader::new(file);
    let mut scanned = Scan::new(recent);
    let chunks = match resume {
        Some(resume) => {
            let chunks = verified(&mut reader, path, resume)?;
            scanned.consumed = resume.state.consumed;
            scanned.lines = resume.state.lines;
            scanned.malformed = resume.state.malformed;
            chunks
        }
        None => Chunks::default(),
    };
    reader
        .seek(SeekFrom::Start(scanned.consumed))
        .map_err(|e| io(path, e))?;
    scanned.hasher = Some(chunks);
    scan(&mut reader, path, &mut scanned, visit)?;
    Ok(scanned)
}

fn verified(reader: &mut BufReader<&File>, path: &Path, resume: &Resume) -> Result<Chunks> {
    let state = &resume.state;
    let count = state.chunk_count();
    if state.chunks.len() as u64 != count {
        return Err(resume_rejected());
    }
    let last = count.saturating_sub(1);
    let partial = state.chunk_len(last) < PREFIX_CHUNK;
    let mut current = Sha256::new();
    for chunk in state.checked_chunks(resume.check) {
        reader
            .seek(SeekFrom::Start(chunk * PREFIX_CHUNK))
            .map_err(|e| io(path, e))?;
        let mut hasher = Sha256::new();
        let index = usize::try_from(chunk).map_err(|_| resume_rejected())?;
        if !hash_prefix(reader, path, &mut hasher, state.chunk_len(chunk))?
            || hex(hasher.clone().finalize()) != state.chunks[index]
        {
            return Err(resume_rejected());
        }
        if chunk == last {
            current = hasher;
        }
    }
    let complete =
        usize::try_from(if partial { last } else { count }).map_err(|_| resume_rejected())?;
    Ok(Chunks {
        digests: state.chunks[..complete].to_vec(),
        current: if partial { current } else { Sha256::new() },
        filled: if partial { state.chunk_len(last) } else { 0 },
        spot_checked: state.spot_checked_after(resume.check),
    })
}

fn hash_prefix(
    reader: &mut dyn BufRead,
    path: &Path,
    hasher: &mut Sha256,
    length: u64,
) -> Result<bool> {
    let mut remaining = length;
    while remaining > 0 {
        let buffer = reader.fill_buf().map_err(|e| io(path, e))?;
        if buffer.is_empty() {
            return Ok(false);
        }
        let take = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        hasher.update(&buffer[..take]);
        reader.consume(take);
        remaining -= take as u64;
    }
    Ok(true)
}

struct Frames<R> {
    source: R,
    decoder: FrameDecoder,
    active: bool,
}

impl<R: BufRead> Read for Frames<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.active {
                while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                    self.decoder
                        .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                        .map_err(std::io::Error::other)?;
                }
                let read = self.decoder.read(buf)?;
                if read > 0 || buf.is_empty() {
                    return Ok(read);
                }
                self.active = false;
            }
            if self.source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            match self.decoder.reset(&mut self.source) {
                Ok(()) => self.active = true,
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    let skipped = std::io::copy(
                        &mut (&mut self.source).take(length.into()),
                        &mut std::io::sink(),
                    )?;
                    if skipped < u64::from(length) {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                }
                Err(e) => return Err(std::io::Error::other(e)),
            }
        }
    }
}
