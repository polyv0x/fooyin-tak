// SPDX-License-Identifier: GPL-3.0-or-later
//! Native TAK PCM decoding from files and sequential sources, without codec libraries.
//! Samples are interleaved, signed, and right aligned in `i32` containers.
mod bits;
mod codec;
mod codes;
pub mod ffi;
mod header;
mod input;
mod legacy;
mod legacy_codes;
mod md5;
mod metadata;

use bits::Bits;
use header::{read_frame_header, Format, FrameHeader};
use input::Input;
use md5::Md5;
pub use metadata::{EncoderInfo, Metadata, WaveMetadata};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Seek};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Invalid(&'static str),
    Unsupported(&'static str),
    ResourceLimit,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "TAK input error: {error}"),
            Self::Invalid(message) => write!(f, "Invalid TAK stream: {message}"),
            Self::Unsupported(message) => write!(f, "Unsupported TAK stream: {message}"),
            Self::ResourceLimit => f.write_str("TAK stream exceeds decoder resource limits"),
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Sample frames count time instants, not individual channel samples.
/// Channel positions use TAK's 1..18 speaker identifiers; zero means unspecified.
/// A zero total means unknown until the final frame is found.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamInfo {
    pub total_samples: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub channel_layout: [u8; 16],
}
impl StreamInfo {
    pub fn duration_ms(self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            self.total_samples * 1000 / u64::from(self.sample_rate)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub enum RecoveryMode {
    #[default]
    Strict = 0,
    Silence = 1,
    Skip = 2,
}

#[derive(Clone, Copy, Debug)]
pub struct DecoderOptions {
    pub verify_md5: bool,
    pub recovery: RecoveryMode,
}
impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            verify_md5: true,
            recovery: RecoveryMode::Strict,
        }
    }
}

/// Verification describes the current traversal. A seek to zero starts afresh;
/// other nontrivial seeks and recovered damage make that traversal Incomplete.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
    NotPresent = 0,
    Disabled = 1,
    Pending = 2,
    Verified = 3,
    Mismatch = 4,
    Incomplete = 5,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Damage {
    pub start: u64,
    pub samples: u64,
}

/// Retains one decoded frame, optional container metadata, and a lazy seek index.
/// Sequential input needs no seek implementation and does not buffer the file.
/// Frame CRCs are checked before returning PCM. Strict decoding is the default.
pub struct Decoder<R> {
    input: Input<R>,
    format: Format,
    metadata: Metadata,
    options: DecoderOptions,
    data_start: u64,
    data_end: Option<u64>,
    exact_end: bool,
    offsets: BTreeMap<u32, u64>,
    decoded: Vec<i32>,
    loaded_frame: Option<u32>,
    position: u64,
    next_frame: Option<u32>,
    seek_table: BTreeMap<u32, u64>,
    failed: bool,
    gap_end: u64,
    damage: Vec<Damage>,
    hash: Md5,
    verification: Verification,
}

fn u24(data: &[u8]) -> usize {
    usize::from(data[0]) | (usize::from(data[1]) << 8) | (usize::from(data[2]) << 16)
}

impl<R: Read + Seek> Decoder<R> {
    pub fn new(source: R) -> Result<Self> {
        Self::with_options(source, DecoderOptions::default())
    }
    pub fn with_options(source: R, options: DecoderOptions) -> Result<Self> {
        Self::open(Input::seekable(source)?, options)
    }
}

impl<R: Read> Decoder<R> {
    /// Open a pipe or other source which starts at the TAK signature.
    /// Reads must block until bytes, EOF, or an error are available; zero means EOF.
    pub fn new_stream(source: R) -> Result<Self> {
        Self::stream_with_options(source, DecoderOptions::default())
    }
    pub fn stream_with_options(source: R, options: DecoderOptions) -> Result<Self> {
        Self::open(Input::stream(source), options)
    }

    fn open(mut input: Input<R>, options: DecoderOptions) -> Result<Self> {
        let container = metadata::Container::read(&mut input)?;
        let start = input.position;
        let data_end = if let Some((offset, size)) = container.last_frame {
            let end = start + offset + size;
            if size < 11 {
                return Err(Error::Invalid("invalid last-frame size"));
            }
            if input.size.is_some_and(|file_size| end > file_size)
                && (options.recovery == RecoveryMode::Strict
                    || container.format.info.total_samples == 0)
            {
                return Err(Error::Invalid("last frame extends past input"));
            }
            Some(end)
        } else {
            input.size
        };
        let verification = initial_verification(&container.metadata, options);
        let mut decoder = Self {
            input,
            format: container.format,
            metadata: container.metadata,
            options,
            data_start: start,
            data_end,
            exact_end: container.last_frame.is_some(),
            offsets: BTreeMap::from([(0, start)]),
            decoded: Vec::new(),
            loaded_frame: None,
            position: 0,
            next_frame: Some(0),
            seek_table: BTreeMap::new(),
            failed: false,
            gap_end: 0,
            damage: Vec::new(),
            hash: Md5::default(),
            verification,
        };
        decoder.read_seek_table(&container.seek_table)?;
        if decoder.is_seekable() {
            if let Some((offset, _)) = container.last_frame {
                decoder.input.seek_to(start + offset)?;
                let result = read_frame_header(&mut Bits::new(&mut decoder.input), &decoder.format)
                    .and_then(|last| {
                        decoder.validate_header(&last)?;
                        if !last.last {
                            return Err(Error::Invalid("invalid last-frame header"));
                        }
                        Ok(last)
                    });
                match result {
                    Ok(last) => {
                        decoder.index(last.number, start + offset);
                        decoder.format.info.total_samples = decoder.frame_end(&last);
                    }
                    Err(error)
                        if options.recovery != RecoveryMode::Strict
                            && decoder.length_known()
                            && recoverable(&error) => {}
                    Err(error) => return Err(error),
                }
            } else if !decoder.length_known() {
                // Unknown length without a last-frame block: scan CRC-checked
                // headers in increasing order, retaining only a bounded index.
                let mut first = 0;
                loop {
                    let (offset, header) = decoder
                        .scan_header(first, None)?
                        .ok_or(Error::Invalid("final frame not found"))?;
                    decoder.index(header.number, offset);
                    if header.last {
                        decoder.format.info.total_samples = decoder.frame_end(&header);
                        break;
                    }
                    first = header.number + 1;
                    decoder.input.seek_to(offset + 2)?;
                }
            }
            decoder.input.seek_to(start)?;
        }
        Ok(decoder)
    }

    pub fn info(&self) -> StreamInfo {
        self.format.info
    }
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    pub fn codec(&self) -> u32 {
        self.format.codec
    }
    /// Significant topmost PCM bits declared by WAVEFORMATEXTENSIBLE, or the
    /// container precision when the extension is absent. Samples are unchanged.
    pub fn valid_bits(&self) -> u16 {
        self.format.valid_bits
    }
    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn is_seekable(&self) -> bool {
        self.input.is_seekable()
    }
    pub fn length_known(&self) -> bool {
        self.info().total_samples != 0
    }
    pub fn verification(&self) -> Verification {
        self.verification
    }
    /// Concealed sample-frame ranges observed during this traversal. A seek to
    /// zero clears them; repeated/overlapping ranges are merged.
    pub fn damage(&self) -> &[Damage] {
        &self.damage
    }

    fn index(&mut self, frame: u32, offset: u64) {
        if self.is_seekable()
            && (self.offsets.len() < MAX_INDEX || self.offsets.contains_key(&frame))
        {
            self.offsets.insert(frame, offset);
        }
    }

    fn read_seek_table(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() < 4 {
            return Err(Error::Invalid("seek table size"));
        }
        let descriptor = u32::from_le_bytes(data[..4].try_into().unwrap());
        let count = descriptor & ((1 << 21) - 1);
        let interval = (descriptor >> 21) + 1;
        if data.len() != 4 + count as usize * 5 {
            return Err(Error::Invalid("seek table size"));
        }
        let stride = (count as usize).div_ceil(MAX_INDEX).max(1);
        let mut previous = 0;
        for (i, bytes) in data[4..].chunks_exact(5).enumerate() {
            let frame = (i as u32 + 1)
                .checked_mul(interval)
                .ok_or(Error::Invalid("seek table frame number"))?;
            let relative =
                u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], 0, 0, 0]);
            let offset = self.data_start + relative;
            if relative <= previous
                || frame >= 1 << 21
                || (self.length_known()
                    && u64::from(frame) * u64::from(self.format.frame_samples)
                        >= self.info().total_samples)
                || self.data_end.is_some_and(|end| offset >= end)
            {
                return Err(Error::Invalid(
                    "seek table entry out of bounds or out of order",
                ));
            }
            previous = relative;
            if self.is_seekable() && i % stride == 0 {
                // Metadata CRC and bounds do not authenticate the pointed-to
                // frame. Check its header CRC and number when this hint is used.
                self.seek_table.insert(frame, offset);
            }
        }
        Ok(())
    }

    /// Sample-accurate seek, clamped to known EOF. Frame decoding stays lazy.
    /// Unsupported on sequential input; physical seek failures are reported.
    pub fn seek(&mut self, sample: u64) -> Result<()> {
        if !self.is_seekable() {
            return Err(Error::Unsupported("input is not seekable"));
        }
        let sample = if self.length_known() {
            sample.min(self.info().total_samples)
        } else {
            sample
        };
        if sample == self.position && !self.failed {
            return Ok(());
        }
        let frame = sample / u64::from(self.format.frame_samples);
        // Locate the requested frame now so an I/O callback failure cannot be
        // reported as a successful seek. PCM decoding itself stays lazy.
        self.next_frame = None;
        if self.loaded_frame != u32::try_from(frame).ok() || self.failed {
            self.loaded_frame = None;
            let located = if self.length_known() && sample == self.info().total_samples {
                Ok(None)
            } else {
                self.locate(frame as u32).and_then(|found| {
                    if found != Some(frame as u32) && self.options.recovery == RecoveryMode::Strict
                    {
                        Err(Error::Invalid("requested frame not found"))
                    } else {
                        Ok(found)
                    }
                })
            };
            if let Err(error) = located {
                self.next_frame = None;
                self.failed = true;
                self.invalidate_verification();
                return Err(error);
            }
        }
        if sample == 0 {
            self.hash = Md5::default();
            self.verification = initial_verification(&self.metadata, self.options);
            self.damage.clear();
        } else {
            self.invalidate_verification();
        }
        self.failed = false;
        self.position = sample;
        self.gap_end = 0;
        Ok(())
    }
    pub fn seek_ms(&mut self, milliseconds: u64) -> Result<()> {
        let sample = u128::from(milliseconds) * u128::from(self.info().sample_rate) / 1000;
        self.seek(sample.min(u128::from(u64::MAX)) as u64)
    }

    fn frame_end(&self, header: &FrameHeader) -> u64 {
        u64::from(header.number) * u64::from(self.format.frame_samples) + u64::from(header.samples)
    }
    fn validate_header(&self, header: &FrameHeader) -> Result<()> {
        let total = self.info().total_samples;
        if total != 0 {
            let end = self.frame_end(header);
            if end > total || (header.last && end != total) || (!header.last && end == total) {
                return Err(Error::Invalid(
                    "frame number or final sample count mismatch",
                ));
            }
        }
        Ok(())
    }

    // Leave input positioned at a CRC-authenticated candidate. The bounded
    // overlap/pushback also works on pipes; recovery has a separate scan budget.
    fn scan_header(
        &mut self,
        first: u32,
        budget: Option<u64>,
    ) -> Result<Option<(u64, FrameHeader)>> {
        let scan_start = self.input.position;
        let mut buffer = [0; 64 * 1024];
        loop {
            let offset = self.input.position;
            let mut limit = self.data_end.map_or(buffer.len(), |end| {
                end.saturating_sub(offset).min(buffer.len() as u64) as usize
            });
            if let Some(budget) = budget {
                let remaining = budget.saturating_sub(offset - scan_start);
                if remaining == 0 {
                    return Err(Error::ResourceLimit);
                }
                limit = limit.min(remaining as usize);
            }
            let mut size = 0;
            while size < limit {
                match self.input.read(&mut buffer[size..limit]) {
                    Ok(0) => break,
                    Ok(count) => size += count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            for i in 0..size.saturating_sub(4) {
                if buffer[i..i + 2] != [0xff, 0xa0] {
                    continue;
                }
                let number = u24(&buffer[i + 2..i + 5]) as u32 >> 3;
                if number < first {
                    continue;
                }
                let header = match read_frame_header(&mut Bits::new(&buffer[i..size]), &self.format)
                {
                    Ok(header) => header,
                    // Unsupported is returned only after the header CRC passes.
                    // Do not disguise format changes as a recoverable dropout.
                    Err(error @ Error::Unsupported(_)) => return Err(error),
                    Err(_) => continue,
                };
                if self.validate_header(&header).is_err() {
                    continue;
                }
                self.input.unread(&buffer[i..size]);
                return Ok(Some((offset + i as u64, header)));
            }
            if size == limit
                && budget.is_some_and(|budget| offset - scan_start + size as u64 >= budget)
                && !self.data_end.is_some_and(|end| offset + size as u64 >= end)
            {
                return Err(Error::ResourceLimit);
            }
            if size < limit || size <= 64 {
                return Ok(None);
            }
            self.input.unread(&buffer[size - 64..size]);
        }
    }

    // Returns the first available frame at or after the requested one. A gap
    // is only accepted by load_frame in explicitly selected recovery modes.
    fn locate(&mut self, frame: u32) -> Result<Option<u32>> {
        if self.next_frame == Some(frame) {
            return Ok(Some(frame));
        }
        if !self.is_seekable() {
            return Err(Error::Unsupported(
                "cannot rewind sequential input after an error",
            ));
        }
        self.next_frame = None;
        let (&mut_known, &mut_start) = self
            .offsets
            .range(..=frame)
            .next_back()
            .ok_or(Error::Invalid("missing first frame"))?;
        let (mut known, mut start) = (mut_known, mut_start);
        if let Some((&hint_frame, &hint_offset)) = self.seek_table.range(..=frame).next_back() {
            if hint_frame > known {
                self.input.seek_to(hint_offset)?;
                let result = read_frame_header(&mut Bits::new(&mut self.input), &self.format)
                    .and_then(|candidate| {
                        self.validate_header(&candidate)?;
                        if candidate.number != hint_frame {
                            return Err(Error::Invalid("seek table points to the wrong frame"));
                        }
                        Ok(())
                    });
                match result {
                    Ok(()) => {
                        self.index(hint_frame, hint_offset);
                        known = hint_frame;
                        start = hint_offset;
                    }
                    Err(error)
                        if self.options.recovery != RecoveryMode::Strict && recoverable(&error) =>
                    {
                        self.seek_table.remove(&hint_frame);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        self.input.seek_to(start)?;
        if known == frame {
            self.next_frame = Some(frame);
            return Ok(Some(frame));
        }
        self.input.seek_to(start + 2)?;
        let mut first = known + 1;
        while let Some((offset, header)) = self.scan_header(first, None)? {
            self.index(header.number, offset);
            if header.number >= frame {
                self.next_frame = Some(header.number);
                return Ok(Some(header.number));
            }
            first = header.number + 1;
            self.input.seek_to(offset + 2)?;
        }
        Ok(None)
    }

    fn load_frame(&mut self, frame: u32) -> Result<()> {
        self.loaded_frame = None;
        let found = self.locate(frame)?;
        if found != Some(frame) {
            if self.options.recovery == RecoveryMode::Strict {
                return Err(Error::Invalid("requested frame not found"));
            }
            let end = match found {
                Some(next) => u64::from(next) * u64::from(self.format.frame_samples),
                None if self.length_known() => self.info().total_samples,
                None => return Err(Error::Invalid("final frame not found")),
            };
            return self.conceal_to(end);
        }
        let offset = self.input.position;
        // A failed read leaves the byte cursor partway through a frame. Never
        // mistake that cursor for the beginning of a subsequent retry.
        self.next_frame = None;
        let mut captured = Vec::new();
        let record = !self.is_seekable() && self.options.recovery != RecoveryMode::Strict;
        let limit = self.data_end.map_or(MAX_FRAME_BYTES, |end| {
            end.saturating_sub(offset).min(MAX_FRAME_BYTES)
        });
        let result = (|| {
            let mut input = Capture {
                input: (&mut self.input).take(limit),
                bytes: &mut captured,
                enabled: record,
            };
            let mut bits = Bits::new(&mut input);
            let header = read_frame_header(&mut bits, &self.format)?;
            if header.number != frame {
                if header.number > frame && self.options.recovery != RecoveryMode::Strict {
                    return Ok((header, None));
                }
                return Err(Error::Invalid("unexpected frame number"));
            }
            let decoded = codec::decode(&mut bits, &self.format, header.samples as usize);
            Ok((header, Some(decoded)))
        })();
        let result = result.and_then(|(header, decoded)| {
            self.validate_header(&header)?;
            if header.last {
                self.format.info.total_samples = self.frame_end(&header);
            }
            let Some(decoded) = decoded else {
                if self.is_seekable() {
                    self.input.seek_to(offset)?;
                } else {
                    self.input.unread(&captured);
                }
                self.next_frame = Some(header.number);
                self.index(header.number, offset);
                return self
                    .conceal_to(u64::from(header.number) * u64::from(self.format.frame_samples));
            };
            let decoded = decoded?;
            if header.last && self.exact_end && Some(self.input.position) != self.data_end {
                return Err(Error::Invalid("final frame size mismatch"));
            }
            self.index(frame, offset);
            self.next_frame = Some(frame + 1);
            if !header.last {
                self.index(frame + 1, self.input.position);
            }
            self.decoded = decoded;
            self.loaded_frame = Some(frame);
            Ok(())
        });
        if let Err(error) = result {
            if self.options.recovery == RecoveryMode::Strict || !recoverable(&error) {
                return Err(error);
            }
            if self.is_seekable() {
                self.input.seek_to(offset + 1)?;
            } else if !captured.is_empty() {
                self.input.unread(&captured[1..]);
            }
            let next = self.scan_header(frame + 1, Some(MAX_FRAME_BYTES))?;
            let end = if let Some((offset, header)) = next {
                self.index(header.number, offset);
                self.next_frame = Some(header.number);
                u64::from(header.number) * u64::from(self.format.frame_samples)
            } else if self.length_known() {
                self.info().total_samples
            } else {
                return Err(error);
            };
            self.conceal_to(end)?;
        }
        Ok(())
    }

    fn invalidate_verification(&mut self) {
        if self.metadata.md5.is_some() && self.options.verify_md5 {
            self.verification = Verification::Incomplete;
        }
    }
    fn conceal_to(&mut self, end: u64) -> Result<()> {
        if end <= self.position {
            return Err(Error::Invalid("non-increasing damaged range"));
        }
        self.invalidate_verification();
        self.record_damage(self.position, end - self.position)?;
        self.gap_end = end;
        Ok(())
    }
    fn record_damage(&mut self, mut start: u64, mut samples: u64) -> Result<()> {
        let mut at = 0;
        while at < self.damage.len() {
            let previous = self.damage[at];
            if previous.start > start + samples {
                break;
            }
            if previous.start + previous.samples >= start {
                let end = (start + samples).max(previous.start + previous.samples);
                start = start.min(previous.start);
                samples = end - start;
                self.damage.remove(at);
            } else {
                at += 1;
            }
        }
        if self.damage.len() >= MAX_DAMAGE {
            return Err(Error::ResourceLimit);
        }
        self.damage.insert(at, Damage { start, samples });
        Ok(())
    }

    /// Read at most one TAK frame, returning a whole number of channel samples.
    /// Errors invalidate the output buffer and latch until a successful seek.
    /// Recovery explicitly silences or skips reported damaged ranges. A missing
    /// final frame cannot be concealed unless its sample count is known.
    pub fn read_samples(&mut self, output: &mut [i32]) -> Result<usize> {
        if self.verification == Verification::Mismatch {
            return Err(Error::Invalid("PCM MD5 mismatch"));
        }
        if self.failed {
            return Err(Error::Invalid("previous decode failed; seek required"));
        }
        if output.is_empty() {
            return Ok(0);
        }
        if output.len() < usize::from(self.info().channels) {
            return Err(Error::Invalid("output buffer cannot hold a sample frame"));
        }
        let result = self.read_inner(output);
        if result.is_err() {
            self.failed = true;
            if self.verification != Verification::Mismatch {
                self.invalidate_verification();
            }
        }
        result
    }

    fn read_inner(&mut self, output: &mut [i32]) -> Result<usize> {
        let channels = usize::from(self.info().channels);
        loop {
            if self.length_known() && self.position >= self.info().total_samples {
                return Ok(0);
            }
            if self.position < self.gap_end {
                if self.options.recovery == RecoveryMode::Skip {
                    self.position = self.gap_end;
                    continue;
                }
                let count = ((self.gap_end - self.position).min((output.len() / channels) as u64)
                    as usize)
                    * channels;
                output[..count].fill(0);
                self.position += (count / channels) as u64;
                return Ok(count);
            }
            let frame = self.position / u64::from(self.format.frame_samples);
            if frame >= 1 << 21 {
                return Err(Error::Invalid(
                    "frame numbering exhausted without final frame",
                ));
            }
            if self.loaded_frame != Some(frame as u32) {
                self.load_frame(frame as u32)?;
                if self.position < self.gap_end {
                    continue;
                }
            }
            let offset = (self.position % u64::from(self.format.frame_samples)) as usize * channels;
            let available = self
                .decoded
                .len()
                .checked_sub(offset)
                .ok_or(Error::Invalid("sample offset exceeds frame"))?;
            let count = (output.len() / channels * channels).min(available);
            if count == 0 {
                return Err(Error::Invalid("frame ended without a final header"));
            }
            output[..count].copy_from_slice(&self.decoded[offset..offset + count]);
            if self.verification == Verification::Pending {
                self.hash.pcm(&output[..count], self.info().bits_per_sample);
            }
            self.position += (count / channels) as u64;
            if self.position == self.info().total_samples
                && self.verification == Verification::Pending
            {
                if Some(self.hash.clone().finish()) != self.metadata.md5 {
                    self.verification = Verification::Mismatch;
                    return Err(Error::Invalid("PCM MD5 mismatch"));
                }
                self.verification = Verification::Verified;
            }
            return Ok(count);
        }
    }
}

const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INDEX: usize = 8192;
const MAX_DAMAGE: usize = 4096;

fn initial_verification(metadata: &Metadata, options: DecoderOptions) -> Verification {
    if metadata.md5.is_none() {
        Verification::NotPresent
    } else if options.verify_md5 {
        Verification::Pending
    } else {
        Verification::Disabled
    }
}
fn recoverable(error: &Error) -> bool {
    matches!(error, Error::Invalid(_))
        || matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof)
}
struct Capture<'a, R> {
    input: R,
    bytes: &'a mut Vec<u8>,
    enabled: bool,
}
impl<R: Read> Read for Capture<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = self.input.read(output)?;
        if self.enabled {
            self.bytes.extend_from_slice(&output[..count]);
        }
        Ok(count)
    }
}
