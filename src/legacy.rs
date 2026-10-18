// SPDX-License-Identifier: GPL-3.0-or-later
// Codec type 0 reconstruction adapted from Librempeg, Copyright 2012 Paul B Mahol.
// Rust adaptation Copyright 2026 polyv0x. See LICENSES/librempeg-tak.txt.
use crate::{bits::Bits, header::Format, legacy_codes::YCODES, Error, Result};
use std::io::Read;

const ORDERS: [usize; 16] = [
    4, 8, 12, 16, 24, 32, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256,
];
const LAST_CODE: i32 = 50;

struct Positions {
    offsets: Vec<usize>,
    resolution: u32,
    minimum: usize,
}

impl Positions {
    fn new(rate: usize, size: usize, mut resolution: u32, maximum: usize, align: usize) -> Self {
        while resolution > 0 && rate / (64 << resolution) < align {
            resolution -= 1;
        }
        let mut offsets = vec![0];
        for index in 1..=maximum {
            let position = (rate * index / (64 << resolution) + align / 2) & !(align - 1);
            offsets.push(position);
            if position >= size {
                break;
            }
        }
        let last = offsets.len() - 1;
        if offsets[last] > size && size - offsets[last - 1] < align {
            offsets.pop();
        }
        *offsets.last_mut().unwrap() = size;
        Self {
            offsets,
            resolution,
            minimum: align,
        }
    }

    fn partition(&self, size: usize, resolution: u32) -> Result<Vec<usize>> {
        if size > *self.offsets.last().unwrap() || resolution > self.resolution {
            return Err(Error::Invalid("legacy partition size"));
        }
        let shift = self.resolution - resolution;
        // The reference's estimate/correction loop computes this same floor.
        let floor = self
            .offsets
            .partition_point(|&p| p <= size)
            .saturating_sub(1);
        let mut blocks = floor >> shift;
        if blocks == 0 || size - self.offsets[blocks << shift] >= self.minimum {
            blocks += 1;
        }
        let mut positions = Vec::with_capacity(blocks + 1);
        for block in 0..blocks {
            positions.push(self.offsets[block << shift]);
        }
        positions.push(size);
        Ok(positions)
    }
}

struct Parameters {
    bps: u32,
    frame: Vec<usize>,
    entropy: Positions,
    resolution: u32,
}

impl Parameters {
    fn new(format: &Format, count: usize) -> Result<Self> {
        let rate = format.info.sample_rate as usize;
        let size = (format.frame_samples as usize).max(count);
        let frame = Positions::new(rate, size, 2, 64, 8);
        let entropy = Positions::new(rate, size, 3, 128, 4);
        let bps = u32::from(format.info.bits_per_sample);
        let thresholds = if bps == 8 {
            [16000, 32000, 48000]
        } else {
            [11025, 22050, 44100]
        };
        let resolution = thresholds.iter().filter(|&&r| rate >= r).count() as u32;
        Ok(Self {
            bps,
            frame: frame.partition(count, frame.resolution)?,
            resolution: resolution.min(entropy.resolution),
            entropy,
        })
    }
}

fn valid_code(code: i32) -> Result<i32> {
    if !(0..=LAST_CODE).contains(&code) {
        Err(Error::Invalid("legacy entropy code"))
    } else {
        Ok(code)
    }
}

fn code_sequence<R: Read>(bits: &mut Bits<R>, count: usize) -> Result<Vec<i32>> {
    let mode = bits.read(2)?;
    if mode > 2 {
        return Err(Error::Invalid("legacy code sequence mode"));
    }
    let mut code = valid_code(bits.read(6)? as i32)?;
    let mut codes = Vec::with_capacity(count);
    if mode == 0 {
        let width = bits.read(3)? + 1;
        for _ in 0..count {
            codes.push(valid_code(code + bits.read(width)? as i32)?);
        }
    } else {
        codes.push(code);
        for _ in 1..count {
            if mode == 1 {
                let delta = bits.unary(9)? as i32;
                if delta == 9 {
                    code = bits.read(6)? as i32;
                } else if delta != 0 {
                    code += if bits.bit()? { -delta } else { delta };
                }
            } else {
                match bits.read(2)? {
                    0 => {
                        let delta = bits.unary(7)? as i32;
                        if delta == 7 {
                            code = bits.read(6)? as i32;
                        } else {
                            code += if bits.bit()? { -(delta + 2) } else { delta + 2 };
                        }
                    }
                    2 => code -= 1,
                    3 => code += 1,
                    _ => {}
                }
            }
            codes.push(valid_code(code)?);
        }
    }
    Ok(codes)
}

fn bits_for_values(count: u32) -> u32 {
    if count <= 2 {
        1
    } else {
        32 - (count - 1).leading_zeros()
    }
}

fn maximum_bits(mut difference: u32, code: i32) -> u32 {
    if code >= 4 {
        let [_, _, _, _, step, end] = YCODES[(code - 4) as usize];
        difference *= 2;
        if difference >= end as u32 + step as u32 {
            bits_for_values((difference - end as u32) / step as u32 + 1) + 10
        } else {
            10
        }
    } else if code != 0 {
        let end = [4, 5, 7][(code - 1) as usize];
        if difference > end {
            1 + bits_for_values(difference - end)
        } else {
            1
        }
    } else {
        1
    }
}

fn entropy_block<R: Read>(
    bits: &mut Bits<R>,
    output: &mut [i32],
    code: i32,
    max_bits: u32,
) -> Result<()> {
    match code {
        0 => output.fill(0),
        1..=3 => {
            let minimum = 1 << (code - 1);
            for sample in output {
                let value = bits.read(code as u32)? as i32;
                *sample = if value != 0 {
                    if value <= minimum {
                        value - 1
                    } else {
                        minimum - value
                    }
                } else {
                    let stop = bits.unary(4)? as i32;
                    if stop < 4 {
                        let value = stop + minimum;
                        if bits.bit()? {
                            -value
                        } else {
                            value
                        }
                    } else {
                        if max_bits == 0 || max_bits > 29 {
                            return Err(Error::Invalid("legacy entropy escape width"));
                        }
                        let value = bits.read(max_bits)? as i32;
                        let negative = 1 << (max_bits - 1);
                        if value < negative {
                            minimum + 4 + value
                        } else {
                            -(minimum + 4 + value - negative)
                        }
                    }
                };
            }
        }
        4..=LAST_CODE => {
            let [width, threshold, small, medium, step, end] = YCODES[(code - 4) as usize];
            let medium_add = (medium >> 1) - threshold;
            let end_add = (end >> 1) - threshold;
            let multiplier = step >> 1;
            for sample in output {
                let mut value = bits.read(width as u32)? as i32;
                *sample = if value >= small {
                    let sign = -(bits.read(1)? as i32);
                    if value >= threshold {
                        let stop = bits.unary(9)? as i32;
                        if stop < 9 {
                            value += medium_add + stop * multiplier;
                        } else {
                            value += end_add;
                            if max_bits > 10 {
                                value = value.wrapping_add(
                                    (bits.read(max_bits - 10)? as i32).wrapping_mul(multiplier),
                                );
                            }
                        }
                    } else {
                        value += (small >> 1) - small;
                    }
                    value ^ sign
                } else {
                    (value >> 1) ^ -(value & 1)
                };
            }
        }
        _ => return Err(Error::Invalid("legacy entropy code")),
    }
    Ok(())
}

fn residuals<R: Read>(bits: &mut Bits<R>, output: &mut [i32], params: &Parameters) -> Result<()> {
    if !bits.bit()? {
        let code = valid_code(bits.read(6)? as i32)?;
        let max_bits = if code == 0 { 0 } else { bits.read(5)? };
        if max_bits > 28 {
            return Err(Error::Invalid("legacy residual width"));
        }
        entropy_block(bits, output, code, max_bits)
    } else {
        let positions = params.entropy.partition(output.len(), params.resolution)?;
        let count = positions.len() - 1;
        if !(2..=128).contains(&count) {
            return Err(Error::Invalid("legacy entropy partition count"));
        }
        let codes = code_sequence(bits, count)?;
        let width = bits.read(5)?;
        if width > 28 {
            return Err(Error::Invalid("legacy residual width"));
        }
        let difference = if width == 0 { 0 } else { 1 << width };
        for (bounds, code) in positions.windows(2).zip(codes) {
            if bounds[0] >= bounds[1] {
                return Err(Error::Invalid("legacy entropy partition size"));
            }
            entropy_block(
                bits,
                &mut output[bounds[0]..bounds[1]],
                code,
                maximum_bits(difference, code),
            )?;
        }
        Ok(())
    }
}

fn integrate(output: &mut [i32], mode: u32) {
    if mode == 1 {
        let mut sum = 0i32;
        for sample in output {
            sum = sum.wrapping_add(*sample);
            *sample = sum;
        }
    } else if mode == 2 && output.len() >= 2 {
        let mut delta = output[1];
        let mut sum = output[0].wrapping_add(delta);
        output[1] = sum;
        for sample in &mut output[2..] {
            delta = delta.wrapping_add(*sample);
            sum = sum.wrapping_add(delta);
            *sample = sum;
        }
    }
}

fn delta<R: Read>(bits: &mut Bits<R>, output: &mut [i32], params: &Parameters) -> Result<()> {
    let mode = bits.read(2)?;
    if mode > 2 {
        return Err(Error::Invalid("legacy delta mode"));
    }
    residuals(bits, output, params)?;
    integrate(output, mode);
    Ok(())
}

struct Predictor {
    coefficients: Vec<i32>,
    sample_shift: u32,
    prediction_shift: u32,
}

impl Predictor {
    fn read<R: Read>(bits: &mut Bits<R>, order: usize, sample_shift: u32) -> Result<Self> {
        let prediction_shift = 10 - bits.read(3)?;
        let differential = order >= 12 && bits.bit()?;
        let (maximum, width) = if order > 16 {
            (bits.read(4)?, 2 + bits.read(1)?)
        } else {
            (0, 0)
        };
        let mut coefficients = Vec::with_capacity(order);
        for _ in 0..order / 4 {
            let resolution = if order <= 16 {
                bits.read(4)?
            } else {
                maximum
                    .checked_sub(bits.read(width)?)
                    .ok_or(Error::Invalid("legacy coefficient width"))?
            };
            if resolution == 0 {
                return Err(Error::Invalid("legacy coefficient width"));
            }
            for _ in 0..4 {
                coefficients.push(bits.signed(resolution)?);
            }
        }
        if differential {
            for i in 1..order {
                coefficients[i] -= coefficients[i - 1];
            }
            if coefficients.iter().any(|c| c.abs() >= 8192) {
                return Err(Error::Invalid("legacy coefficient range"));
            }
        }
        Ok(Self {
            coefficients,
            sample_shift,
            prediction_shift,
        })
    }

    fn apply(&self, output: &mut [i32]) -> Result<()> {
        let order = self.coefficients.len();
        if output.len() < order {
            return Err(Error::Invalid("legacy predictor order"));
        }
        let mut history = vec![0; output.len()];
        for (value, &sample) in history.iter_mut().zip(&output[..order]) {
            *value = shift_down(sample, self.sample_shift);
        }
        for i in order..output.len() {
            let sum = dot(&history[i - order..i], &self.coefficients);
            output[i] = prediction(sum, self.prediction_shift)
                .wrapping_shl(self.sample_shift)
                .wrapping_sub(output[i]);
            history[i] = shift_down(output[i], self.sample_shift);
        }
        Ok(())
    }
}

// TAK rounds the magnitude toward zero, rather than using an arithmetic shift.
fn shift_down(value: i32, shift: u32) -> i32 {
    let sign = value >> 31;
    (((value.wrapping_add(sign) ^ sign) >> shift) ^ sign).wrapping_sub(sign)
}

fn prediction(sum: i32, shift: u32) -> i32 {
    let sign = sum >> 31;
    let magnitude = (sum.wrapping_add(sign) ^ sign).wrapping_add(1 << (shift - 1)) >> shift;
    (magnitude.min(8191) ^ sign).wrapping_sub(sign)
}

fn dot(values: &[i32], coefficients: &[i32]) -> i32 {
    values
        .iter()
        .zip(coefficients)
        .fold(0i32, |sum, (&v, &c)| sum.wrapping_add(v.wrapping_mul(c)))
}

fn subframe<R: Read>(bits: &mut Bits<R>, output: &mut [i32], params: &Parameters) -> Result<()> {
    if bits.bit()? {
        let shift = bits.read(4)?;
        let order = ORDERS[bits.read(4)? as usize];
        if order > output.len() {
            return Err(Error::Invalid("legacy predictor order"));
        }
        delta(bits, &mut output[..order], params)?;
        let predictor = Predictor::read(bits, order, shift)?;
        residuals(bits, &mut output[order..], params)?;
        predictor.apply(output)
    } else {
        delta(bits, output, params)
    }
}

struct Channel {
    wasted: u32,
    prefilter: Option<Predictor>,
}

impl Channel {
    fn read<R: Read>(bits: &mut Bits<R>, output: &mut [i32], params: &Parameters) -> Result<Self> {
        let wasted = bits.escape4()?;
        if wasted >= params.bps {
            return Err(Error::Invalid("legacy wasted bits"));
        }
        output[0] = bits.signed(params.bps - wasted)?.wrapping_shl(wasted);
        let prefilter = if bits.bit()? {
            let shift = bits.read(4)?;
            if bits.read(2)? != 0 {
                return Err(Error::Invalid("legacy prefilter type"));
            }
            Some(Predictor::read(bits, 4, shift)?)
        } else {
            None
        };
        let count = bits.read(3)? as usize + 1;
        if count > 5 {
            return Err(Error::Invalid("legacy subframe count"));
        }
        let mut positions = vec![0];
        let mut previous = 0;
        for _ in 1..count {
            let block = bits.read(6)? as usize;
            if block <= previous || block >= params.frame.len() - 1 {
                return Err(Error::Invalid("legacy subframe position"));
            }
            positions.push(params.frame[block]);
            previous = block;
        }
        positions.push(output.len() - 1);
        for bounds in positions.windows(2) {
            if bounds[0] >= bounds[1] || bounds[1] >= output.len() {
                return Err(Error::Invalid("legacy subframe size"));
            }
            subframe(bits, &mut output[1 + bounds[0]..1 + bounds[1]], params)?;
        }
        Ok(Self { wasted, prefilter })
    }

    fn finish(&self, output: &mut [i32]) -> Result<()> {
        if let Some(predictor) = &self.prefilter {
            predictor.apply(&mut output[1..])?;
        }
        for sample in &mut output[1..] {
            *sample = sample.wrapping_shl(self.wasted);
        }
        integrate(output, 1);
        Ok(())
    }
}

struct Joint {
    predictor: Predictor,
    start: bool,
    end: bool,
}

impl Joint {
    fn read<R: Read>(bits: &mut Bits<R>) -> Result<Self> {
        let start = bits.bit()?;
        let end = bits.bit()?;
        let shift = bits.read(4)?;
        if bits.read(2)? != 1 {
            return Err(Error::Invalid("legacy joint predictor type"));
        }
        Ok(Self {
            predictor: Predictor::read(bits, 8, shift)?,
            start,
            end,
        })
    }

    fn apply(&self, source: &[i32], target: &mut [i32]) -> Result<()> {
        if source.len() < 256 {
            return Err(Error::Invalid("legacy joint predictor size"));
        }
        let count = source.len() - 7;
        if self.start {
            for i in 0..4 {
                target[i] = target[i].wrapping_add(source[i]);
            }
        }
        if self.end {
            for i in 4 + count..source.len() {
                target[i] = target[i].wrapping_add(source[i]);
            }
        }
        let history: Vec<i32> = source
            .iter()
            .map(|&v| shift_down(v, self.predictor.sample_shift))
            .collect();
        for i in 0..count {
            let sum = dot(&history[i..i + 8], &self.predictor.coefficients);
            // Joint prediction always uses ten fractional bits.
            target[i + 4] = prediction(sum, 10)
                .wrapping_shl(self.predictor.sample_shift)
                .wrapping_sub(target[i + 4]);
        }
        Ok(())
    }
}

pub(crate) fn decode<R: Read>(
    bits: &mut Bits<R>,
    format: &Format,
    channels: &mut [Vec<i32>],
) -> Result<()> {
    let params = Parameters::new(format, channels[0].len())?;
    if channels.len() == 1 {
        let channel = Channel::read(bits, &mut channels[0], &params)?;
        return channel.finish(&mut channels[0]);
    }
    let mode = bits.read(3)?;
    if mode > 5 {
        return Err(Error::Invalid("legacy joint stereo mode"));
    }
    let first = usize::from(mode == 2 || mode == 4);
    let main = Channel::read(bits, &mut channels[first], &params)?;
    let joint = if mode == 1 || mode == 2 {
        Some(Joint::read(bits)?)
    } else {
        None
    };
    let other = Channel::read(bits, &mut channels[1 - first], &params)?;
    if mode >= 3 && (main.prefilter.is_some() || other.prefilter.is_some()) {
        return Err(Error::Invalid("legacy joint stereo prefilter"));
    }
    let (left, right) = channels.split_at_mut(1);
    let a = &mut left[0][1..];
    let b = &mut right[0][1..];
    if let Some(joint) = joint {
        if first == 0 {
            joint.apply(a, b)?;
        } else {
            joint.apply(b, a)?;
        }
    } else {
        for (a, b) in a.iter_mut().zip(b) {
            match mode {
                3 => *b = b.wrapping_add(*a),
                4 => *a = b.wrapping_sub(*a),
                5 => {
                    let side = *b;
                    let mid = a.wrapping_mul(2).wrapping_add(side & 1);
                    *a = mid.wrapping_sub(side) >> 1;
                    *b = mid.wrapping_add(side) >> 1;
                }
                _ => {}
            }
        }
    }
    main.finish(&mut channels[first])?;
    other.finish(&mut channels[1 - first])
}
