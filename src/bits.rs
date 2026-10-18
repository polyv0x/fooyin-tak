// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded little-endian bit input and TAK's CRC-24/OpenPGP checksums.
use crate::{Error, Result};
use std::io::Read;

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 16;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc << 1) ^ if crc & 0x800000 != 0 { 0x864cfb } else { 0 };
            bit += 1;
        }
        table[i] = crc & 0xffffff;
        i += 1;
    }
    table
}
const CRC_TABLE: [u32; 256] = crc_table();

fn update(crc: u32, byte: u8) -> u32 {
    ((crc << 8) ^ CRC_TABLE[((crc >> 16) as u8 ^ byte) as usize]) & 0xffffff
}

pub(crate) fn check_crc(data: &[u8]) -> Result<()> {
    if data.len() < 3 {
        return Err(Error::Invalid("missing checksum"));
    }
    let n = data.len() - 3;
    let expected = u32::from_le_bytes([data[n], data[n + 1], data[n + 2], 0]);
    let actual = data[..n].iter().fold(0xb704ce, |crc, &b| update(crc, b));
    if expected != actual {
        return Err(Error::Invalid("checksum mismatch"));
    }
    Ok(())
}

pub(crate) struct Bits<R> {
    input: R,
    cache: u64,
    available: u32,
    crc: u32,
    bytes: usize,
}

impl<R: Read> Bits<R> {
    pub fn new(input: R) -> Self {
        Self {
            input,
            cache: 0,
            available: 0,
            crc: 0xb704ce,
            bytes: 0,
        }
    }

    pub fn read(&mut self, count: u32) -> Result<u32> {
        Ok(self.wide(count)? as u32)
    }

    pub fn wide(&mut self, count: u32) -> Result<u64> {
        debug_assert!(count <= 40);
        while self.available < count {
            if self.bytes >= 16 * 1024 * 1024 {
                return Err(Error::ResourceLimit);
            }
            let mut byte = [0];
            self.input.read_exact(&mut byte)?;
            self.crc = update(self.crc, byte[0]);
            self.cache |= u64::from(byte[0]) << self.available;
            self.available += 8;
            self.bytes += 1;
        }
        let value = self.cache & ((1u64 << count) - 1);
        self.cache >>= count;
        self.available -= count;
        Ok(value)
    }

    pub fn signed(&mut self, count: u32) -> Result<i32> {
        if count == 0 || count > 32 {
            return Err(Error::Invalid("invalid signed code width"));
        }
        Ok((self.read(count)? << (32 - count)) as i32 >> (32 - count))
    }

    pub fn bit(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }

    pub fn unary(&mut self, limit: u32) -> Result<u32> {
        let mut value = 0;
        while value < limit && !self.bit()? {
            value += 1;
        }
        Ok(value)
    }

    pub fn escape4(&mut self) -> Result<u32> {
        if self.bit()? {
            Ok(self.read(4)? + 1)
        } else {
            Ok(0)
        }
    }

    pub fn align(&mut self) {
        self.cache = 0;
        self.available = 0;
    }

    pub fn finish_crc(&mut self) -> Result<()> {
        self.align();
        let actual = self.crc;
        let expected = self.read(24)?;
        self.crc = 0xb704ce;
        if expected != actual {
            return Err(Error::Invalid("checksum mismatch"));
        }
        Ok(())
    }
}
