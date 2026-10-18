// SPDX-License-Identifier: GPL-3.0-or-later
// Format interpretation adapted from FFmpeg's tak.c, Copyright 2012 Paul B Mahol.
// See LICENSES/ffmpeg-tak.txt.
use crate::{bits::Bits, Error, Result, StreamInfo};
use std::io::Read;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Format {
    pub info: StreamInfo,
    pub codec: u32,
    pub frame_samples: u32,
}

impl Format {
    pub fn read<R: Read>(bits: &mut Bits<R>) -> Result<Self> {
        let codec = bits.read(6)?;
        bits.read(4)?; // Encoder profile.
        let frame_type = bits.read(4)? as usize;
        let total_samples = bits.wide(35)?;
        let data_type = bits.read(3)?;
        let sample_rate = bits.read(18)? + 6000;
        let bits_per_sample = bits.read(5)? as u16 + 8;
        let channels = bits.read(4)? as u16 + 1;
        let mut channel_layout = [0; 16];
        if bits.bit()? {
            bits.read(5)?; // Valid bits (container precision remains bits_per_sample).
            if bits.bit()? {
                for position in &mut channel_layout[..channels as usize] {
                    let value = bits.read(6)? as u8;
                    if value > 18 {
                        return Err(Error::Unsupported("speaker position"));
                    }
                    *position = value;
                }
            }
        }
        if !matches!(codec, 0 | 2 | 4) {
            return Err(Error::Unsupported("codec version (expected 0, 2 or 4)"));
        }
        if data_type != 0 {
            return Err(Error::Unsupported("non-PCM audio"));
        }
        if !matches!(bits_per_sample, 8 | 16 | 24) {
            return Err(Error::Unsupported(
                "only 8-, 16-, and 24-bit PCM is supported",
            ));
        }
        if codec != 4 && channels > 2 {
            return Err(Error::Invalid("mono/stereo codec with too many channels"));
        }
        let quants = [3, 4, 6, 8, 4096, 8192, 16384, 512, 1024, 2048];
        let &quant = quants
            .get(frame_type)
            .ok_or(Error::Invalid("frame duration"))?;
        let frame_samples = if frame_type < 4 {
            sample_rate * quant / 32
        } else {
            quant
        };
        if frame_samples == 0
            || frame_samples > 16384
            || (frame_type >= 4 && frame_samples > sample_rate / 4)
        {
            return Err(Error::Invalid("frame sample count"));
        }
        if total_samples.div_ceil(u64::from(frame_samples)) > 1 << 21 {
            return Err(Error::Invalid("sample count exceeds frame numbering range"));
        }
        Ok(Self {
            info: StreamInfo {
                sample_rate,
                total_samples,
                channels,
                bits_per_sample,
                channel_layout,
            },
            codec,
            frame_samples,
        })
    }
}

pub(crate) struct FrameHeader {
    pub number: u32,
    pub samples: u32,
    pub last: bool,
}

pub(crate) fn read_frame_header<R: Read>(
    bits: &mut Bits<R>,
    format: &Format,
) -> Result<FrameHeader> {
    if bits.read(16)? != 0xa0ff {
        return Err(Error::Invalid("missing frame sync"));
    }
    let flags = bits.read(3)?;
    let number = bits.read(21)?;
    let last = flags & 1 != 0;
    let samples = if last {
        let samples = bits.read(14)? + 1;
        bits.read(2)?;
        samples
    } else {
        format.frame_samples
    };
    if samples > format.frame_samples {
        return Err(Error::Invalid("oversized final frame"));
    }
    if flags & 2 != 0 {
        let in_frame = Format::read(bits)?;
        // Encoders may write a zero (unknown) length in repeated stream headers.
        let mut comparable = in_frame;
        comparable.info.total_samples = format.info.total_samples;
        if comparable != *format {
            return Err(Error::Unsupported("changing stream format"));
        }
        if bits.read(6)? != 0 {
            bits.read(25)?;
        }
        bits.align();
    }
    if flags & 4 != 0 {
        return Err(Error::Unsupported("metadata inside audio frames"));
    }
    bits.finish_crc()?;
    Ok(FrameHeader {
        number,
        samples,
        last,
    })
}
