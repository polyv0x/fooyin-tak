// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{bits, header::Format, input::Input, u24, Error, Result};
use std::io::{self, Read};

/// Encoder settings, when stored. Version is 0xMMmmpp (major/minor/patch).
/// Evaluation 0/1/2 means standard/extra/maximum; None means not recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderInfo {
    pub version: u32,
    pub preset: u8,
    pub evaluation: Option<u8>,
}

/// Original bytes preceding and following the WAV PCM payload, without audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveMetadata {
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub encoder: Option<EncoderInfo>,
    pub md5: Option<[u8; 16]>,
    pub wave: Option<WaveMetadata>,
}

pub(crate) struct Container {
    pub format: Format,
    pub metadata: Metadata,
    pub last_frame: Option<(u64, u64)>,
    pub seek_table: Vec<u8>,
}

impl Container {
    pub fn read<R: Read>(input: &mut Input<R>) -> Result<Self> {
        let mut signature = [0; 4];
        input.read_exact(&mut signature)?;
        if &signature != b"tBaK" {
            return Err(Error::Invalid("missing tBaK signature"));
        }
        let mut format = None;
        let mut metadata = Metadata::default();
        let mut last_frame = None;
        let mut seek_table = Vec::new();
        let mut seen = [false; 8];
        let mut retained = 0;
        let mut metadata_bytes = 0u64;
        let mut blocks = 0;
        loop {
            let mut header = [0; 4];
            input.read_exact(&mut header)?;
            let kind = (header[0] & 127) as usize;
            let size = u24(&header[1..]);
            metadata_bytes += 4 + size as u64;
            blocks += 1;
            if metadata_bytes > 64 * 1024 * 1024 || blocks > 1024 {
                return Err(Error::ResourceLimit);
            }
            if input
                .size
                .is_some_and(|end| input.position + size as u64 > end)
            {
                return Err(Error::Invalid("truncated metadata"));
            }
            if kind == 0 {
                if size != 0 {
                    return Err(Error::Invalid("nonempty end metadata block"));
                }
                break;
            }
            if matches!(kind, 1..=4 | 6 | 7) {
                if seen[kind] {
                    return Err(Error::Invalid("duplicate metadata block"));
                }
                seen[kind] = true;
                if size < 4 || (matches!(kind, 1 | 4 | 6 | 7) && size > 1024) {
                    return Err(Error::Invalid("metadata block size"));
                }
                retained += size;
                if retained > 32 * 1024 * 1024 {
                    return Err(Error::ResourceLimit);
                }
                let mut data = vec![0; size];
                input.read_exact(&mut data)?;
                bits::check_crc(&data)?;
                data.truncate(size - 3);
                match kind {
                    1 => format = Some(Format::read(&mut bits::Bits::new(data.as_slice()))?),
                    2 => seek_table = data,
                    3 => {
                        if data.len() < 6 {
                            return Err(Error::Invalid("WAV metadata size"));
                        }
                        let head = u24(&data[..3]);
                        let tail = u24(&data[3..6]);
                        if head + tail != data.len() - 6 {
                            return Err(Error::Invalid("WAV metadata size"));
                        }
                        metadata.wave = Some(WaveMetadata {
                            head: data[6..6 + head].to_vec(),
                            tail: data[6 + head..].to_vec(),
                        });
                    }
                    4 => {
                        if data.len() != 4 {
                            return Err(Error::Invalid("encoder metadata size"));
                        }
                        metadata.encoder = Some(EncoderInfo {
                            version: u24(&data[..3]) as u32,
                            preset: data[3] & 15,
                            evaluation: Some((data[3] >> 4) & 3),
                        });
                    }
                    6 => {
                        metadata.md5 = Some(
                            data.as_slice()
                                .try_into()
                                .map_err(|_| Error::Invalid("MD5 metadata size"))?,
                        )
                    }
                    7 => {
                        if data.len() != 8 {
                            return Err(Error::Invalid("last-frame metadata size"));
                        }
                        let mut bits = bits::Bits::new(data.as_slice());
                        last_frame = Some((bits.wide(40)?, u64::from(bits.read(24)?)));
                    }
                    _ => unreachable!(),
                }
            } else {
                // Padding and unknown extensions can be skipped without retaining
                // them, including on nonseekable inputs. Do not accept short reads.
                let copied = io::copy(&mut input.take(size as u64), &mut io::sink())?;
                if copied != size as u64 {
                    return Err(Error::Invalid("truncated metadata"));
                }
            }
        }
        let format = format.ok_or(Error::Invalid("missing stream information"))?;
        if metadata.encoder.is_none() && format.codec == 0 {
            metadata.encoder = Some(EncoderInfo {
                version: 0x10000,
                preset: format.profile as u8,
                evaluation: None,
            });
        }
        Ok(Self {
            format,
            metadata,
            last_frame,
            seek_table,
        })
    }
}
