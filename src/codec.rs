// SPDX-License-Identifier: GPL-3.0-or-later
// TAK reconstruction adapted from FFmpeg's takdec.c and takdsp.c.
// Copyright (c) 2012 Paul B Mahol. Rust adaptation Copyright (c) 2026 polyv0x.
// See LICENSES/ffmpeg-tak.txt for the original LGPL-2.1-or-later notice.
use crate::{bits::Bits, codes::CODES, header::Format, Error, Result};
use std::io::Read;

struct Parameters {
    window: usize,
    subframe_scale: usize,
    bps: u32,
}

impl Parameters {
    fn new(format: &Format) -> Self {
        let rate = format.info.sample_rate;
        let shift = if rate < 11025 {
            3
        } else if rate < 22050 {
            2
        } else if rate < 44100 {
            1
        } else {
            0
        };
        let base = (rate.div_ceil(512) + 3) & !3;
        Self {
            window: (base << shift) as usize,
            subframe_scale: (base * 2) as usize,
            bps: u32::from(format.info.bits_per_sample),
        }
    }
}

fn lpc(samples: &mut [i32], mode: u32) {
    if samples.len() < 2 || mode == 0 {
        return;
    }
    let mut value = samples[0];
    let mut first = samples[1];
    value = value.wrapping_add(first);
    samples[1] = value;
    if mode == 1 {
        for sample in &mut samples[2..] {
            value = value.wrapping_add(*sample);
            *sample = value;
        }
    } else if mode == 2 {
        for sample in &mut samples[2..] {
            first = first.wrapping_add(*sample);
            value = value.wrapping_add(first);
            *sample = value;
        }
    } else if samples.len() > 2 {
        let mut second = samples[2];
        first = first.wrapping_add(second);
        value = value.wrapping_add(first);
        samples[2] = value;
        for sample in &mut samples[3..] {
            second = second.wrapping_add(*sample);
            first = first.wrapping_add(second);
            value = value.wrapping_add(first);
            *sample = value;
        }
    }
}

fn segment<R: Read>(bits: &mut Bits<R>, samples: &mut [i32], mode: i32) -> Result<()> {
    if mode == 0 {
        samples.fill(0);
        return Ok(());
    }
    let &[initial, escape, scale, advanced_escape, bias] = CODES
        .get(mode.wrapping_sub(1) as usize)
        .ok_or(Error::Invalid("residual coding mode"))?;
    for sample in samples {
        let mut x = bits.read(initial)?;
        if x >= escape && bits.bit()? {
            x |= 1 << initial;
            if x >= advanced_escape {
                let mut n = bits.unary(9)?;
                if n == 9 {
                    let mut width = bits.read(3)?;
                    if width > 0 {
                        if width == 7 {
                            width += bits.read(5)?;
                            if width > 29 {
                                return Err(Error::Invalid("residual escape width"));
                            }
                        }
                        n = bits.read(width)? + 1;
                        x = x.wrapping_add(scale.wrapping_mul(n));
                    }
                    x = x.wrapping_add(bias);
                } else {
                    x = x.wrapping_add(scale.wrapping_mul(n)).wrapping_sub(escape);
                }
            } else {
                x -= escape;
            }
        }
        *sample = ((x >> 1) as i32) ^ -((x & 1) as i32);
    }
    Ok(())
}

fn residues<R: Read>(bits: &mut Bits<R>, samples: &mut [i32], params: &Parameters) -> Result<()> {
    if !bits.bit()? {
        let mode = bits.read(6)? as i32;
        return segment(bits, samples, mode);
    }
    let mut windows = samples.len() / params.window;
    let mut remainder = samples.len() % params.window;
    if remainder < params.window / 2 {
        remainder += params.window;
    } else {
        windows += 1;
    }
    if !(2..=128).contains(&windows) {
        return Err(Error::Invalid("residual window count"));
    }
    let mut modes = [0i32; 128];
    let mut mode = bits.read(6)? as i32;
    modes[0] = mode;
    for slot in &mut modes[1..windows] {
        match bits.unary(6)? {
            0 => {}
            1 => mode -= 1,
            2 => mode += 1,
            c @ 3..=5 => {
                mode += if bits.bit()? {
                    1 - c as i32
                } else {
                    c as i32 - 1
                }
            }
            _ => mode = bits.read(6)? as i32,
        }
        *slot = mode;
    }
    let mut offset = 0;
    for (index, &mode) in modes[..windows].iter().enumerate() {
        let size = if index == windows - 1 {
            remainder
        } else {
            params.window
        };
        segment(bits, &mut samples[offset..offset + size], mode)?;
        offset += size;
    }
    Ok(())
}

fn subframe<R: Read>(
    bits: &mut Bits<R>,
    samples: &mut [i32],
    mut start: usize,
    size: usize,
    previous: usize,
    params: &Parameters,
) -> Result<()> {
    let end = start + size;
    if !bits.bit()? {
        return residues(bits, &mut samples[start..end], params);
    }
    let orders = [
        4, 8, 12, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 224, 256, 0,
    ];
    let order = orders[bits.read(4)? as usize];
    if order == 0 {
        return Err(Error::Invalid("zero prediction order"));
    }
    if previous > 0 && bits.bit()? {
        if order > previous {
            return Err(Error::Invalid("prediction exceeds previous subframe"));
        }
        start -= order;
    } else {
        if order > size {
            return Err(Error::Invalid("prediction exceeds subframe"));
        }
        let mode = bits.read(2)?;
        if mode > 2 {
            return Err(Error::Invalid("prediction warmup mode"));
        }
        residues(bits, &mut samples[start..start + order], params)?;
        lpc(&mut samples[start..start + order], mode);
    }
    let shift = bits.escape4()?;
    let width = bits.read(1)? + 6;
    let quant = if bits.bit()? { 9 - bits.read(3)? } else { 10 };
    if quant < 3 {
        return Err(Error::Invalid("filter quantization"));
    }
    let mut predictors = [0i32; 256];
    predictors[0] = bits.signed(10)?;
    predictors[1] = bits.signed(10)?;
    predictors[2] = bits.signed(width)? << (10 - width);
    predictors[3] = bits.signed(width)? << (10 - width);
    if order > 4 {
        let base = width - bits.read(1)?;
        let mut n = 0;
        for (i, value) in predictors.iter_mut().enumerate().take(order).skip(4) {
            if i & 3 == 0 {
                n = base - bits.read(2)?;
            }
            *value = bits.signed(n)? << (10 - width);
        }
    }
    let mut temporary = [0i32; 256];
    temporary[0] = predictors[0] * 64;
    for i in 1..order {
        for j in 0..i.div_ceil(2) {
            let k = i - 1 - j;
            let a = temporary[j];
            let b = temporary[k];
            temporary[k] = b.wrapping_add(predictors[i].wrapping_mul(a).wrapping_add(256) >> 9);
            temporary[j] = a.wrapping_add(predictors[i].wrapping_mul(b).wrapping_add(256) >> 9);
        }
        temporary[i] = predictors[i] * 64;
    }
    let rounding = 1 << (14 - quant);
    let mut filter = [0i16; 256];
    for i in 0..order {
        // FFmpeg's added 1 << (17 + quant) vanishes on conversion to int16.
        filter[order - 1 - i] =
            (0i32.wrapping_sub(temporary[i].wrapping_add(rounding) >> (15 - quant))) as i16;
    }
    residues(bits, &mut samples[start + order..end], params)?;
    let mut history = [0i16; 544];
    for i in 0..order {
        history[i] = (samples[start + i] >> shift) as i16;
    }
    let mut position = start + order;
    while position < end {
        let size = (544 - order).min(end - position);
        for i in 0..size {
            let mut value = 1i32 << (quant - 1);
            for j in 0..order {
                value = value.wrapping_add(i32::from(history[i + j]) * i32::from(filter[j]));
            }
            value = (value >> quant)
                .clamp(-8192, 8191)
                .wrapping_shl(shift)
                .wrapping_sub(samples[position + i]);
            samples[position + i] = value;
            history[order + i] = (value >> shift) as i16;
        }
        history.copy_within(size..size + order, 0);
        position += size;
    }
    Ok(())
}

fn channel<R: Read>(
    bits: &mut Bits<R>,
    samples: &mut [i32],
    params: &Parameters,
) -> Result<(u32, u32)> {
    let shift = bits.escape4()?;
    if shift >= params.bps {
        return Err(Error::Invalid("channel sample shift"));
    }
    samples[0] = bits.signed(params.bps - shift)?;
    let mode = bits.read(2)?;
    let count = bits.read(3)? as usize + 1;
    let mut lengths = [0; 8];
    let mut left = samples.len() - 1;
    let mut previous = 0;
    for length in &mut lengths[..count - 1] {
        let value = bits.read(6)? as usize;
        if value <= previous {
            return Err(Error::Invalid("subframe boundary order"));
        }
        *length = (value - previous) * params.subframe_scale;
        if *length >= left {
            return Err(Error::Invalid("subframe length"));
        }
        left -= *length;
        previous = value;
    }
    lengths[count - 1] = left;
    let mut start = 1;
    previous = 0;
    for &length in &lengths[..count] {
        subframe(bits, samples, start, length, previous, params)?;
        start += length;
        previous = length;
    }
    Ok((mode, shift))
}

fn pair(channels: &mut [Vec<i32>], a: usize, b: usize) -> (&mut [i32], &mut [i32]) {
    if a < b {
        let (left, right) = channels.split_at_mut(b);
        (&mut left[a], &mut right[0])
    } else {
        let (left, right) = channels.split_at_mut(a);
        (&mut right[0], &mut left[b])
    }
}

fn decorrelate<R: Read>(
    bits: &mut Bits<R>,
    channels: &mut [Vec<i32>],
    mut a: usize,
    mut b: usize,
    mode: u32,
) -> Result<()> {
    if mode == 0 {
        return Ok(());
    }
    if a == b {
        return Err(Error::Invalid("self-referencing channel"));
    }
    if matches!(mode, 4 | 6) {
        std::mem::swap(&mut a, &mut b);
    }
    let (p1, p2) = pair(channels, a, b);
    let length = p1.len();
    match mode {
        1..=3 => {
            for i in 1..length {
                match mode {
                    1 => p2[i] = p1[i].wrapping_add(p2[i]),
                    2 => p1[i] = p2[i].wrapping_sub(p1[i]),
                    _ => {
                        p1[i] = p1[i].wrapping_sub(p2[i] >> 1);
                        p2[i] = p1[i].wrapping_add(p2[i]);
                    }
                }
            }
        }
        4 | 5 => {
            let shift = bits.escape4()?;
            let factor = bits.signed(10)?;
            for i in 1..length {
                let value = factor.wrapping_mul(p2[i] >> shift).wrapping_add(128) >> 8;
                p1[i] = value.wrapping_shl(shift).wrapping_sub(p1[i]);
            }
        }
        6 | 7 => {
            let p1 = &mut p1[1..];
            let p2 = &p2[1..];
            let length = p1.len();
            if length < 256 {
                return Err(Error::Invalid("channel filter on short frame"));
            }
            let shift = bits.escape4()?;
            let order = 8usize << bits.read(1)?;
            let begin = bits.bit()?;
            let end = bits.bit()?;
            let mut filter = [0i32; 16];
            let mut width = 0;
            for (i, value) in filter.iter_mut().enumerate().take(order) {
                if i & 3 == 0 {
                    width = 14 - bits.read(3)?;
                }
                *value = bits.signed(width)?;
            }
            let half = order / 2;
            let filtered = length - (order - 1);
            if begin {
                for i in 0..half {
                    p1[i] = p1[i].wrapping_add(p2[i]);
                }
            }
            if end {
                for i in filtered + half..length {
                    p1[i] = p1[i].wrapping_add(p2[i]);
                }
            }
            for i in 0..filtered {
                let mut value = 512i32;
                for j in 0..order {
                    value = value.wrapping_add(i32::from((p2[i + j] >> shift) as i16) * filter[j]);
                }
                p1[i + half] = (value >> 10)
                    .clamp(-8192, 8191)
                    .wrapping_shl(shift)
                    .wrapping_sub(p1[i + half]);
            }
        }
        _ => return Err(Error::Invalid("channel decorrelation mode")),
    }
    Ok(())
}

pub(crate) fn decode<R: Read>(
    bits: &mut Bits<R>,
    format: &Format,
    count: usize,
) -> Result<Vec<i32>> {
    let params = Parameters::new(format);
    let n = usize::from(format.info.channels);
    let mut channels = vec![vec![0; count]; n];
    let mut modes = [(0, 0); 16];
    if count < 16 {
        for samples in &mut channels {
            for sample in samples {
                *sample = bits.signed(params.bps)?;
            }
        }
    } else if format.codec == 0 {
        crate::legacy::decode(bits, format, &mut channels)?;
    } else {
        if format.codec == 2 {
            for (i, samples) in channels.iter_mut().enumerate() {
                modes[i] = channel(bits, samples, &params)?;
            }
            if n == 2 {
                if bits.bit()? {
                    bits.read(6)?;
                }
                let mode = bits.read(3)?;
                decorrelate(bits, &mut channels, 0, 1, mode)?;
            }
        } else {
            let mut entries = Vec::new();
            if bits.bit()? {
                let count = bits.read(4)? as usize + 1;
                if count > n {
                    return Err(Error::Invalid("channel group count"));
                }
                let mut mask = 0u32;
                for _ in 0..count {
                    let a = bits.read(4)? as usize;
                    if a >= n || mask & (1 << a) != 0 {
                        return Err(Error::Invalid("duplicate or invalid channel"));
                    }
                    let present = bits.bit()?;
                    let (index, b) = if present {
                        let index = bits.read(2)? as usize;
                        let b = bits.read(4)? as usize;
                        if b >= n || a == b {
                            return Err(Error::Invalid("channel reference"));
                        }
                        if index == 1 {
                            if mask & (1 << b) != 0 {
                                return Err(Error::Invalid("duplicate channel reference"));
                            }
                            mask |= 1 << b;
                        } else if mask & (1 << b) == 0 {
                            return Err(Error::Invalid("forward channel reference"));
                        }
                        (index, b)
                    } else {
                        (0, 0)
                    };
                    mask |= 1 << a;
                    entries.push((a, b, index, present));
                }
                if mask != (1 << n) - 1 {
                    return Err(Error::Invalid("missing channel"));
                }
            } else {
                for a in 0..n {
                    entries.push((a, 0, 0, false));
                }
            }
            for (a, b, index, present) in entries {
                if present && index == 1 {
                    modes[b] = channel(bits, &mut channels[b], &params)?;
                }
                modes[a] = channel(bits, &mut channels[a], &params)?;
                if present {
                    decorrelate(bits, &mut channels, b, a, [1, 3, 4, 6][index])?;
                }
            }
        }
        for (samples, &(mode, shift)) in channels.iter_mut().zip(&modes) {
            lpc(samples, mode);
            for sample in samples {
                *sample = sample.wrapping_shl(shift);
            }
        }
    }
    bits.finish_crc()?;
    let mut output = Vec::with_capacity(count * n);
    // Keep right-aligned signed PCM in the Rust API. The C ABI scales to S32.
    for i in 0..count {
        for channel in &channels {
            output.push(channel[i].wrapping_shl(32 - params.bps) >> (32 - params.bps));
        }
    }
    Ok(output)
}
