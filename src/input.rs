// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::VecDeque;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

type SeekFn<R> = fn(&mut BufReader<R>, SeekFrom) -> io::Result<u64>;

// Logical position and bounded pushback also work on pipes. Pushback lets error
// recovery inspect bytes consumed by a damaged frame without losing the next one.
pub(crate) struct Input<R> {
    source: BufReader<R>,
    seek: Option<SeekFn<R>>,
    replay: VecDeque<u8>,
    pub position: u64,
    pub size: Option<u64>,
    position_valid: bool,
}

impl<R: Read> Input<R> {
    pub fn stream(source: R) -> Self {
        Self {
            source: BufReader::with_capacity(64 * 1024, source),
            seek: None,
            replay: VecDeque::new(),
            position: 0,
            size: None,
            position_valid: true,
        }
    }

    pub fn is_seekable(&self) -> bool {
        self.seek.is_some()
    }

    pub fn seek_to(&mut self, position: u64) -> io::Result<()> {
        if self.position_valid && self.position == position {
            return Ok(());
        }
        let seek = self
            .seek
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "input is not seekable"))?;
        // A callback may move before reporting failure. Never optimize the
        // next seek away using a cursor whose underlying position is unknown.
        self.position_valid = false;
        if seek(&mut self.source, SeekFrom::Start(position))? != position {
            return Err(io::Error::other("input seek returned the wrong position"));
        }
        self.position = position;
        self.position_valid = true;
        self.replay.clear();
        Ok(())
    }

    pub fn unread(&mut self, bytes: &[u8]) {
        debug_assert!(bytes.len() as u64 <= self.position);
        // A failed frame captures at most 16 MiB, and header scans push back
        // at most 64 KiB. Neither operation can accumulate an entire stream.
        debug_assert!(self.replay.len() + bytes.len() <= 16 * 1024 * 1024 + 64 * 1024);
        self.position -= bytes.len() as u64;
        for &byte in bytes.iter().rev() {
            self.replay.push_front(byte);
        }
    }
}

impl<R: Read + Seek> Input<R> {
    pub fn seekable(mut source: R) -> io::Result<Self> {
        let size = source.seek(SeekFrom::End(0))?;
        if source.seek(SeekFrom::Start(0))? != 0 {
            return Err(io::Error::other("input did not rewind"));
        }
        let mut input = Self::stream(source);
        input.seek = Some(|source, position| source.seek(position));
        input.size = Some(size);
        Ok(input)
    }
}

impl<R: Read> Read for Input<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = if self.replay.is_empty() {
            match self.source.read(output) {
                Ok(count) => count,
                Err(error) => {
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
                    ) {
                        self.position_valid = false;
                    }
                    return Err(error);
                }
            }
        } else {
            let count = output.len().min(self.replay.len());
            for byte in &mut output[..count] {
                *byte = self.replay.pop_front().unwrap();
            }
            count
        };
        self.position += count as u64;
        Ok(count)
    }
}
