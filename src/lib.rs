// SPDX-License-Identifier: GPL-3.0-or-later
//! Native TAK PCM decoding from seekable sources, without codec libraries.
//! Samples are interleaved, signed, and right aligned in `i32` containers.
mod bits;
mod codec;
mod codes;
pub mod ffi;
mod header;
mod legacy;
mod legacy_codes;

use bits::Bits;
use header::{read_frame_header, Format};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

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
        self.total_samples * 1000 / u64::from(self.sample_rate)
    }
}

/// A streaming decoder retaining one decoded frame and a lazy frame-offset index.
/// Forward seeks to unvisited audio scan CRC-validated headers without decoding
/// the intervening samples. Previously visited frames are directly seekable.
pub struct Decoder<R> {
    input: BufReader<R>,
    format: Format,
    data_end: u64,
    has_data_end: bool,
    offsets: BTreeMap<u32, u64>,
    decoded: Vec<i32>,
    loaded_frame: Option<u32>,
    position: u64,
}

fn u24(data: &[u8]) -> usize {
    usize::from(data[0]) | (usize::from(data[1]) << 8) | (usize::from(data[2]) << 16)
}

impl<R: Read + Seek> Decoder<R> {
    /// Validate the container and stream properties. Audio CRCs are checked before
    /// returning PCM. Trailing APEv2/ID3 tags are never interpreted as audio.
    pub fn new(mut source: R) -> Result<Self> {
        let file_size = source.seek(SeekFrom::End(0))?;
        source.seek(SeekFrom::Start(0))?;
        let mut input = BufReader::with_capacity(64 * 1024, source);
        let mut signature = [0; 4];
        input.read_exact(&mut signature)?;
        if &signature != b"tBaK" {
            return Err(Error::Invalid("missing tBaK signature"));
        }
        let mut format = None;
        let mut last_frame = None;
        loop {
            let mut header = [0; 4];
            input.read_exact(&mut header)?;
            let kind = header[0] & 0x7f;
            let size = u24(&header[1..]);
            let offset = input.stream_position()?;
            if offset + size as u64 > file_size {
                return Err(Error::Invalid("truncated metadata"));
            }
            if kind == 0 {
                if size != 0 {
                    return Err(Error::Invalid("nonempty end metadata block"));
                }
                break;
            }
            if matches!(kind, 1 | 4 | 6 | 7) {
                if size < 4 {
                    return Err(Error::Invalid("short metadata block"));
                }
                // These blocks are small even with all sixteen channel positions.
                if size > 1024 {
                    return Err(Error::Invalid("oversized metadata block"));
                }
                let mut data = vec![0; size];
                input.read_exact(&mut data)?;
                bits::check_crc(&data)?;
                match kind {
                    1 => {
                        if format.is_some() {
                            return Err(Error::Invalid("duplicate stream information"));
                        }
                        format = Some(Format::read(&mut Bits::new(&data[..size - 3]))?);
                    }
                    6 if size != 19 => return Err(Error::Invalid("MD5 metadata size")),
                    7 => {
                        if size != 11 || last_frame.is_some() {
                            return Err(Error::Invalid("last-frame metadata"));
                        }
                        let mut bits = Bits::new(&data[..8]);
                        last_frame = Some((bits.wide(40)?, bits.read(24)? as u64));
                    }
                    _ => {}
                }
            } else {
                input.seek(SeekFrom::Start(offset + size as u64))?;
            }
        }
        let mut format = format.ok_or(Error::Invalid("missing stream information"))?;
        let start = input.stream_position()?;
        let mut data_end = file_size;
        let mut offsets = BTreeMap::from([(0, start)]);
        if let Some((offset, size)) = last_frame {
            data_end = start + offset + size;
            if size < 11 || data_end > file_size {
                return Err(Error::Invalid("last frame extends past input"));
            }
            input.seek(SeekFrom::Start(start + offset))?;
            let last = read_frame_header(&mut Bits::new(&mut input), &format)?;
            let total =
                u64::from(last.number) * u64::from(format.frame_samples) + u64::from(last.samples);
            if !last.last || (format.info.total_samples != 0 && total != format.info.total_samples)
            {
                return Err(Error::Invalid("inconsistent final sample count"));
            }
            format.info.total_samples = total;
            offsets.insert(last.number, start + offset);
        }
        if format.info.total_samples == 0 {
            return Err(Error::Unsupported(
                "unknown sample count without last-frame metadata",
            ));
        }
        input.seek(SeekFrom::Start(start))?;
        Ok(Self {
            input,
            format,
            data_end,
            has_data_end: last_frame.is_some(),
            offsets,
            decoded: Vec::new(),
            loaded_frame: None,
            position: 0,
        })
    }

    pub fn info(&self) -> StreamInfo {
        self.format.info
    }
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Sample-accurate seek, clamped to EOF. I/O and validation occur on next read.
    pub fn seek(&mut self, sample: u64) {
        self.position = sample.min(self.info().total_samples);
    }
    pub fn seek_ms(&mut self, milliseconds: u64) {
        let sample = u128::from(milliseconds) * u128::from(self.info().sample_rate) / 1000;
        self.seek(sample.min(u128::from(self.info().total_samples)) as u64);
    }

    fn locate(&mut self, frame: u32) -> Result<u64> {
        if let Some(&offset) = self.offsets.get(&frame) {
            return Ok(offset);
        }
        let (&known, &start) = self
            .offsets
            .range(..frame)
            .next_back()
            .ok_or(Error::Invalid("missing first frame"))?;
        let mut offset = start;
        let mut buffer = vec![0u8; 64 * 1024];
        // A TAK frame header is at most 40 bytes, including all speaker IDs.
        // Retain an overlap so a candidate at a chunk boundary is fully parsed.
        while offset < self.data_end {
            self.input.seek(SeekFrom::Start(offset))?;
            let size = (self.data_end - offset).min(buffer.len() as u64) as usize;
            self.input.read_exact(&mut buffer[..size])?;
            for i in 0..size.saturating_sub(1) {
                if buffer[i] != 0xff || buffer[i + 1] != 0xa0 {
                    continue;
                }
                let Ok(header) = read_frame_header(&mut Bits::new(&buffer[i..size]), &self.format)
                else {
                    continue;
                };
                if header.number <= known || header.number > frame {
                    continue;
                }
                let position = offset + i as u64;
                self.offsets.entry(header.number).or_insert(position);
                if header.number == frame {
                    return Ok(position);
                }
            }
            if offset + size as u64 == self.data_end {
                break;
            }
            offset += (size - 64) as u64;
        }
        Err(Error::Invalid("requested frame not found"))
    }

    /// Read at most one TAK frame, returning the number of channel samples.
    /// A nonempty output buffer must fit a whole sample frame. Corrupt audio is
    /// rejected before any samples from that compressed frame are returned.
    pub fn read_samples(&mut self, output: &mut [i32]) -> Result<usize> {
        if output.is_empty() || self.position == self.info().total_samples {
            return Ok(0);
        }
        let channels = usize::from(self.info().channels);
        if output.len() < channels {
            return Err(Error::Invalid("output buffer cannot hold a sample frame"));
        }
        let frame = (self.position / u64::from(self.format.frame_samples)) as u32;
        if self.loaded_frame != Some(frame) {
            self.loaded_frame = None;
            let offset = self.locate(frame)?;
            self.input.seek(SeekFrom::Start(offset))?;
            let mut bits = Bits::new((&mut self.input).take(self.data_end - offset));
            let header = read_frame_header(&mut bits, &self.format)?;
            let expected = (self.format.info.total_samples
                - u64::from(frame) * u64::from(self.format.frame_samples))
            .min(u64::from(self.format.frame_samples));
            if header.number != frame || u64::from(header.samples) != expected {
                return Err(Error::Invalid("frame number or sample count mismatch"));
            }
            let is_last = u64::from(frame) * u64::from(self.format.frame_samples) + expected
                == self.format.info.total_samples;
            if header.last != is_last {
                return Err(Error::Invalid("unexpected final frame flag"));
            }
            self.decoded = codec::decode(&mut bits, &self.format, header.samples as usize)?;
            let next = self.input.stream_position()?;
            if is_last && self.has_data_end && next != self.data_end {
                return Err(Error::Invalid("final frame size mismatch"));
            }
            if !is_last {
                self.offsets.insert(frame + 1, next);
            }
            self.loaded_frame = Some(frame);
        }
        let offset = (self.position % u64::from(self.format.frame_samples)) as usize * channels;
        let count = (output.len() / channels * channels).min(self.decoded.len() - offset);
        output[..count].copy_from_slice(&self.decoded[offset..offset + count]);
        self.position += (count / channels) as u64;
        Ok(count)
    }
}
