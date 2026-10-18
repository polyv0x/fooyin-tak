// SPDX-License-Identifier: GPL-3.0-or-later
use fooyin_tak::{Damage, Decoder, DecoderOptions, Error, RecoveryMode, Verification};
use std::cell::Cell;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::rc::Rc;

const STEREO: &[u8] = include_bytes!("fixtures/stereo16.tak");
const STEREO_PCM: &[u8] = include_bytes!("fixtures/stereo16.s32le");

fn pcm(bytes: &[u8], bits: u16) -> Vec<i32> {
    bytes
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes(b.try_into().unwrap()) >> (32 - bits))
        .collect()
}
fn all<R: Read>(decoder: &mut Decoder<R>) -> Result<Vec<i32>, Error> {
    let mut result = Vec::new();
    let mut buffer = vec![0; 317 * usize::from(decoder.info().channels) + 1];
    loop {
        let count = decoder.read_samples(&mut buffer)?;
        if count == 0 {
            return Ok(result);
        }
        result.extend_from_slice(&buffer[..count]);
    }
}
fn options(recovery: RecoveryMode) -> DecoderOptions {
    DecoderOptions {
        recovery,
        verify_md5: true,
    }
}
fn crc(data: &[u8]) -> [u8; 3] {
    let mut value = 0xb704ceu32;
    for &byte in data {
        value ^= u32::from(byte) << 16;
        for _ in 0..8 {
            value = ((value << 1) ^ if value & 0x800000 != 0 { 0x864cfb } else { 0 }) & 0xffffff;
        }
    }
    [value as u8, (value >> 8) as u8, (value >> 16) as u8]
}
fn repair_crc(data: &mut [u8]) {
    let end = data.len() - 3;
    let checksum = crc(&data[..end]);
    data[end..].copy_from_slice(&checksum);
}
fn blocks(data: &[u8]) -> Vec<(u8, usize, usize)> {
    let mut result = Vec::new();
    let mut offset = 4;
    loop {
        let kind = data[offset] & 127;
        let size =
            u32::from_le_bytes([data[offset + 1], data[offset + 2], data[offset + 3], 0]) as usize;
        result.push((kind, offset + 4, size));
        if kind == 0 {
            break;
        }
        offset += size + 4;
    }
    result
}
fn block(data: &[u8], kind: u8) -> (usize, usize) {
    blocks(data)
        .into_iter()
        .find_map(|(k, start, size)| (k == kind).then_some((start, size)))
        .unwrap()
}
fn without(data: &[u8], kinds: &[u8]) -> Vec<u8> {
    let mut result = b"tBaK".to_vec();
    for (kind, start, size) in blocks(data) {
        if !kinds.contains(&kind) {
            result.extend_from_slice(&data[start - 4..start + size]);
        }
        if kind == 0 {
            result.extend_from_slice(&data[start..]);
        }
    }
    result
}
fn set_bits(data: &mut [u8], start: usize, count: usize, value: u64) {
    for i in 0..count {
        let bit = start + i;
        data[bit / 8] =
            (data[bit / 8] & !(1 << (bit % 8))) | (((value >> i) as u8 & 1) << (bit % 8));
    }
}
fn unknown_length(data: &[u8]) -> Vec<u8> {
    let mut result = without(data, &[7]);
    let (start, size) = block(&result, 1);
    set_bits(&mut result[start..start + size], 14, 35, 0);
    repair_crc(&mut result[start..start + size]);
    result
}
fn bits(data: &[u8], at: &mut usize, n: usize) -> Option<u32> {
    let mut result = 0;
    for i in 0..n {
        result |= u32::from((*data.get(*at / 8)? >> (*at % 8)) & 1) << i;
        *at += 1;
    }
    Some(result)
}
#[derive(Clone, Copy, Debug)]
struct Header {
    offset: usize,
    size: usize,
    number: u32,
    samples: u32,
    last: bool,
}
fn header(data: &[u8], offset: usize) -> Option<Header> {
    let data = data.get(offset..)?;
    let mut at = 0;
    if bits(data, &mut at, 16)? != 0xa0ff {
        return None;
    }
    let flags = bits(data, &mut at, 3)?;
    let number = bits(data, &mut at, 21)?;
    let last = flags & 1 != 0;
    let samples = if last {
        let n = bits(data, &mut at, 14)? + 1;
        bits(data, &mut at, 2)?;
        n
    } else {
        0
    };
    if flags & 2 != 0 {
        at += 75;
        let channels = bits(data, &mut at, 4)? + 1;
        if bits(data, &mut at, 1)? != 0 {
            at += 5;
            if bits(data, &mut at, 1)? != 0 {
                at += channels as usize * 6;
            }
        }
        if bits(data, &mut at, 6)? != 0 {
            at += 25;
        }
        at = at.div_ceil(8) * 8;
    }
    let size = at.div_ceil(8) + 3;
    let bytes = data.get(..size)?;
    if crc(&bytes[..size - 3]) != bytes[size - 3..] {
        return None;
    }
    Some(Header {
        offset,
        size,
        number,
        samples,
        last,
    })
}
fn headers(data: &[u8]) -> Vec<Header> {
    let start = block(data, 0).0;
    (start..data.len())
        .filter_map(|offset| header(data, offset))
        .collect()
}
fn frame_samples(data: &[u8]) -> u64 {
    let mut decoder = Decoder::new(Cursor::new(data)).unwrap();
    let mut samples = vec![0; 16384 * 16];
    decoder.read_samples(&mut samples).unwrap() as u64 / u64::from(decoder.info().channels)
}

// Deliberately implements no Seek and forces reads across every metadata and
// frame boundary in small chunks, as synchronous pipes and archive devices do.
struct Pipe {
    data: Cursor<Vec<u8>>,
    chunk: usize,
}
impl Pipe {
    fn new(data: Vec<u8>, chunk: usize) -> Self {
        Self {
            data: Cursor::new(data),
            chunk,
        }
    }
}
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = bytes.len().min(self.chunk);
        self.data.read(&mut bytes[..n])
    }
}

#[test]
fn sequential_sources_resolve_unknown_length_without_seek() {
    for data in [STEREO.to_vec(), unknown_length(STEREO)] {
        for chunk in [1, 3, 97, 65536] {
            let unknown = data != STEREO;
            let mut decoder = Decoder::new_stream(Pipe::new(data.clone(), chunk)).unwrap();
            assert!(!decoder.is_seekable());
            assert_eq!(decoder.length_known(), !unknown);
            assert!(decoder.seek(0).is_err());
            assert_eq!(all(&mut decoder).unwrap(), pcm(STEREO_PCM, 16));
            assert!(decoder.length_known());
            assert_eq!(decoder.info().total_samples as usize, STEREO_PCM.len() / 8);
            assert_eq!(decoder.verification(), Verification::Verified);
        }
    }
    let mut decoder = Decoder::new(Cursor::new(unknown_length(STEREO))).unwrap();
    assert!(decoder.length_known());
    decoder.seek(10000).unwrap();
    assert_eq!(all(&mut decoder).unwrap(), pcm(STEREO_PCM, 16)[20000..]);
    assert_eq!(decoder.verification(), Verification::Incomplete);
}

#[test]
fn md5_status_tracks_noops_seeks_resets_and_mismatch() {
    let mut decoder = Decoder::new(Cursor::new(STEREO)).unwrap();
    assert_eq!(decoder.verification(), Verification::Pending);
    decoder.read_samples(&mut [0; 20]).unwrap();
    decoder.seek(decoder.position()).unwrap();
    assert_eq!(decoder.verification(), Verification::Pending);
    all(&mut decoder).unwrap();
    assert_eq!(decoder.verification(), Verification::Verified);
    decoder.seek(decoder.position()).unwrap();
    assert_eq!(decoder.verification(), Verification::Verified);
    decoder.seek(2).unwrap();
    all(&mut decoder).unwrap();
    assert_eq!(decoder.verification(), Verification::Incomplete);
    decoder.seek(0).unwrap();
    assert_eq!(decoder.verification(), Verification::Pending);
    all(&mut decoder).unwrap();
    assert_eq!(decoder.verification(), Verification::Verified);

    let mut corrupt = STEREO.to_vec();
    let (start, size) = block(&corrupt, 6);
    corrupt[start] ^= 1;
    repair_crc(&mut corrupt[start..start + size]);
    let mut decoder = Decoder::new(Cursor::new(&corrupt)).unwrap();
    assert!(all(&mut decoder)
        .unwrap_err()
        .to_string()
        .contains("MD5 mismatch"));
    assert_eq!(decoder.verification(), Verification::Mismatch);
    assert!(decoder.read_samples(&mut [0; 2]).is_err());
    decoder.seek(0).unwrap();
    assert_eq!(decoder.verification(), Verification::Pending);
    assert!(all(&mut decoder).is_err());
    let mut decoder = Decoder::with_options(
        Cursor::new(&corrupt),
        DecoderOptions {
            verify_md5: false,
            recovery: RecoveryMode::Strict,
        },
    )
    .unwrap();
    assert_eq!(all(&mut decoder).unwrap(), pcm(STEREO_PCM, 16));
    assert_eq!(decoder.verification(), Verification::Disabled);
    let mut decoder = Decoder::new(Cursor::new(without(STEREO, &[6]))).unwrap();
    all(&mut decoder).unwrap();
    assert_eq!(decoder.verification(), Verification::NotPresent);
}

#[test]
fn encoder_wave_and_md5_metadata_preserve_original_values() {
    let decoder = Decoder::new(Cursor::new(STEREO)).unwrap();
    let encoder = decoder.metadata().encoder.unwrap();
    assert_eq!(
        (encoder.version, encoder.preset, encoder.evaluation),
        (0x020303, 4, Some(2))
    );
    assert_eq!(
        decoder.metadata().md5.unwrap(),
        [
            0xfc, 0xce, 0x7f, 0x55, 0x2c, 0xad, 0x76, 0x79, 0x49, 0xbb, 0xcc, 0xcd, 0xea, 0x20,
            0xa5, 0x6d
        ]
    );
    let wave = decoder.metadata().wave.as_ref().unwrap();
    assert_eq!(wave.head.len(), 44);
    assert_eq!(&wave.head[..4], b"RIFF");
    assert_eq!(&wave.head[36..40], b"data");
    assert!(wave.tail.is_empty());
    assert_eq!(decoder.valid_bits(), 16);
    let mut mono = Decoder::new(Cursor::new(include_bytes!("fixtures/mono8.tak"))).unwrap();
    assert_eq!(mono.metadata().wave.as_ref().unwrap().tail, [0]);
    all(&mut mono).unwrap();
    assert_eq!(mono.verification(), Verification::Verified);
    let mut surround =
        Decoder::new(Cursor::new(include_bytes!("fixtures/surround24.tak"))).unwrap();
    all(&mut surround).unwrap();
    assert_eq!(surround.verification(), Verification::Verified);
    let legacy = Decoder::new(Cursor::new(include_bytes!("fixtures/legacy-stereo16.tak"))).unwrap();
    let encoder = legacy.metadata().encoder.unwrap();
    assert_eq!(encoder.version, 0x010000);
    assert_eq!(encoder.evaluation, None);
}

#[test]
fn recovery_conceals_crc_damage_and_headers_on_files_and_pipes() {
    let original = without(STEREO, &[7]);
    let frames = headers(&original);
    assert!(frames.len() > 2);
    let per_frame = frame_samples(&original);
    let reference = pcm(STEREO_PCM, 16);
    for damage_header in [false, true] {
        let mut data = original.clone();
        let damaged = 1;
        let at = if damage_header {
            frames[damaged].offset + frames[damaged].size - 1
        } else {
            frames[damaged + 1].offset - 1
        };
        data[at] ^= 1;
        let mut strict = Decoder::new_stream(Pipe::new(data.clone(), 17)).unwrap();
        assert!(all(&mut strict).is_err());
        for mode in [RecoveryMode::Silence, RecoveryMode::Skip] {
            let start = per_frame as usize * 2;
            let end = start * 2;
            let mut expected = reference.clone();
            if mode == RecoveryMode::Silence {
                expected[start..end].fill(0);
            } else {
                expected.drain(start..end);
            }
            for sequential in [false, true] {
                let mut decoder = if sequential {
                    Decoder::stream_with_options(Cursor::new(&data), options(mode)).unwrap()
                } else {
                    Decoder::with_options(Cursor::new(&data), options(mode)).unwrap()
                };
                assert_eq!(
                    all(&mut decoder).unwrap(),
                    expected,
                    "header={damage_header}, sequential={sequential}, mode={mode:?}"
                );
                assert_eq!(
                    decoder.damage(),
                    [Damage {
                        start: per_frame,
                        samples: per_frame
                    }]
                );
                assert_eq!(decoder.verification(), Verification::Incomplete);
                assert_eq!(decoder.position(), decoder.info().total_samples);
                if !sequential {
                    decoder.seek(per_frame + 7).unwrap();
                    assert!(all(&mut decoder).is_ok());
                    assert_eq!(decoder.damage().len(), 1);
                    decoder.seek(0).unwrap();
                    assert!(decoder.damage().is_empty());
                }
            }
        }
    }
}

#[test]
fn missing_frames_recover_from_first_frame_and_from_uncached_seek() {
    let original = without(STEREO, &[7]);
    let frames = headers(&original);
    let per_frame = frame_samples(&original);
    for removed in [0, 1] {
        let mut data = original.clone();
        data.drain(frames[removed].offset..frames[removed + 1].offset);
        let mut expected = pcm(STEREO_PCM, 16);
        let start = removed * per_frame as usize * 2;
        expected[start..start + per_frame as usize * 2].fill(0);
        let mut decoder = Decoder::stream_with_options(
            Pipe::new(data.clone(), 3),
            options(RecoveryMode::Silence),
        )
        .unwrap();
        assert_eq!(all(&mut decoder).unwrap(), expected);
        let mut decoder =
            Decoder::with_options(Cursor::new(&data), options(RecoveryMode::Silence)).unwrap();
        decoder.seek(removed as u64 * per_frame + 7).unwrap();
        assert_eq!(
            all(&mut decoder).unwrap(),
            expected[(removed * per_frame as usize + 7) * 2..]
        );
    }
}

#[test]
fn final_damage_requires_known_sample_count_and_missing_final_flags_fail() {
    let original = without(STEREO, &[7]);
    let frames = headers(&original);
    let last = *frames.last().unwrap();
    assert!(last.last);
    assert!(last.samples > 0);
    let per_frame = frame_samples(&original);
    let last_sample = u64::from(last.number) * per_frame;
    let mut corrupt = original.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    // A valid final header supplies the missing duration even on a pipe.
    let mut decoder = Decoder::stream_with_options(
        Pipe::new(unknown_length(&corrupt), 7),
        options(RecoveryMode::Silence),
    )
    .unwrap();
    let mut expected = pcm(STEREO_PCM, 16);
    expected[last_sample as usize * 2..].fill(0);
    assert_eq!(all(&mut decoder).unwrap(), expected);
    assert_eq!(
        decoder.damage(),
        [Damage {
            start: last_sample,
            samples: u64::from(last.samples)
        }]
    );
    let mut truncated = original[..last.offset].to_vec();
    let mut decoder =
        Decoder::stream_with_options(Cursor::new(&truncated), options(RecoveryMode::Silence))
            .unwrap();
    assert_eq!(all(&mut decoder).unwrap(), expected);
    truncated = unknown_length(&truncated);
    let mut decoder =
        Decoder::stream_with_options(Cursor::new(&truncated), options(RecoveryMode::Silence))
            .unwrap();
    assert!(all(&mut decoder).is_err());
    let mut no_final = original;
    no_final[last.offset + 2] &= !1;
    let mut decoder = Decoder::new_stream(Cursor::new(no_final)).unwrap();
    assert!(all(&mut decoder).is_err());
}

struct Faults {
    data: Cursor<Vec<u8>>,
    fail: Rc<Cell<bool>>,
    interrupted: bool,
}
impl Read for Faults {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.interrupted {
            self.interrupted = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.fail.get() {
            return Err(io::Error::other("cancelled by caller"));
        }
        // Disable BufReader prefetch beyond the requested byte to make the
        // first audio read, rather than open, observe the injected failure.
        let count = output.len().min(1);
        self.data.read(&mut output[..count])
    }
}
impl Seek for Faults {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.fail.get() {
            return Err(io::Error::other("seek cancelled by caller"));
        }
        self.data.seek(position)
    }
}
#[test]
fn recovery_never_swallows_io_failures_and_failed_reads_need_seek() {
    for mode in [
        RecoveryMode::Strict,
        RecoveryMode::Silence,
        RecoveryMode::Skip,
    ] {
        let fail = Rc::new(Cell::new(false));
        let input = Faults {
            data: Cursor::new(STEREO.to_vec()),
            fail: fail.clone(),
            interrupted: true,
        };
        let mut decoder = Decoder::with_options(input, options(mode)).unwrap();
        fail.set(true);
        assert!(matches!(
            decoder.read_samples(&mut [0; 100]),
            Err(Error::Io(_))
        ));
        assert!(decoder.damage().is_empty());
        assert_eq!(decoder.verification(), Verification::Incomplete);
        assert!(decoder.seek(10000).is_err());
        fail.set(false);
        assert!(decoder.read_samples(&mut [0; 100]).is_err());
        decoder.seek(0).unwrap();
        assert_eq!(all(&mut decoder).unwrap(), pcm(STEREO_PCM, 16));
        assert_eq!(decoder.verification(), Verification::Verified);
    }
}

#[test]
fn resynchronization_ignores_false_sync_but_rejects_authenticated_format_changes() {
    let mut original = without(STEREO, &[7]);
    let frames = headers(&original);
    let first = frames[0];
    assert_ne!(original[first.offset + 2] & 2, 0);
    // Embed an otherwise plausible header with a broken CRC inside the damaged
    // frame. It must not create a spurious damage boundary.
    let mut candidate = original[first.offset..first.offset + first.size].to_vec();
    set_bits(&mut candidate, 19, 21, 1);
    let at = first.offset + first.size + 4;
    original[at..at + candidate.len()].copy_from_slice(&candidate);
    original[frames[1].offset - 1] ^= 1;
    let mut decoder =
        Decoder::stream_with_options(Cursor::new(&original), options(RecoveryMode::Silence))
            .unwrap();
    all(&mut decoder).unwrap();
    assert_eq!(decoder.damage()[0].samples, frame_samples(STEREO));

    // The same candidate, with a changed profile and a valid CRC, is a genuine
    // unsupported format transition and must stop recovery.
    set_bits(&mut candidate, 40 + 6, 4, 15);
    repair_crc(&mut candidate);
    original[at..at + candidate.len()].copy_from_slice(&candidate);
    let mut decoder =
        Decoder::stream_with_options(Cursor::new(&original), options(RecoveryMode::Silence))
            .unwrap();
    assert!(matches!(all(&mut decoder), Err(Error::Unsupported(_))));
}

struct Meter {
    data: Cursor<Vec<u8>>,
    read: Rc<Cell<usize>>,
}
impl Read for Meter {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let n = output.len().min(64);
        let n = self.data.read(&mut output[..n])?;
        self.read.set(self.read.get() + n);
        Ok(n)
    }
}
impl Seek for Meter {
    fn seek(&mut self, at: SeekFrom) -> io::Result<u64> {
        self.data.seek(at)
    }
}
#[test]
fn original_tak_1_seek_table_matches_crc_headers_and_accelerates_seeks() {
    let data = include_bytes!("fixtures/legacy-seektable.tak");
    let (start, size) = block(data, 2);
    let descriptor = u32::from_le_bytes(data[start..start + 4].try_into().unwrap());
    let count = descriptor & ((1 << 21) - 1);
    let interval = (descriptor >> 21) + 1;
    assert_eq!((count, interval), (106, 10));
    let audio = block(data, 0).0;
    for (i, entry) in data[start + 4..start + size - 3]
        .chunks_exact(5)
        .enumerate()
    {
        let relative =
            u64::from_le_bytes([entry[0], entry[1], entry[2], entry[3], entry[4], 0, 0, 0]);
        let header = header(data, audio + relative as usize)
            .expect("seek hint must identify a CRC-valid header");
        assert_eq!(header.number, (i as u32 + 1) * interval);
    }
    let mut full = Decoder::new(Cursor::new(data)).unwrap();
    let silence = all(&mut full).unwrap();
    assert_eq!(silence.len(), 800000);
    assert!(silence.iter().all(|&sample| sample == 0));
    let read = Rc::new(Cell::new(0));
    let mut decoder = Decoder::new(Meter {
        data: Cursor::new(data.to_vec()),
        read: read.clone(),
    })
    .unwrap();
    assert_eq!(decoder.info().total_samples, 800000);
    decoder.seek(795007).unwrap();
    let output = all(&mut decoder).unwrap();
    assert_eq!(output, vec![0; 800000 - 795007]);
    assert!(
        read.get() < 4096,
        "read {} bytes instead of using the seek table",
        read.get()
    );
    decoder.seek(750000).unwrap();
    assert!(all(&mut decoder).unwrap().iter().all(|&n| n == 0));
}

#[test]
fn malformed_seek_table_bounds_and_pointed_headers_are_checked() {
    let original = include_bytes!("fixtures/legacy-seektable.tak");
    let (start, size) = block(original, 2);
    for bad in [0u64, u64::MAX >> 24] {
        let mut data = original.to_vec();
        data[start + 4..start + 9].copy_from_slice(&bad.to_le_bytes()[..5]);
        repair_crc(&mut data[start..start + size]);
        assert!(Decoder::new(Cursor::new(data)).is_err());
    }
    // A monotonically increasing, in-bounds offset can still point inside a
    // frame. Metadata CRC alone must never authenticate it.
    let mut data = original.to_vec();
    data[start + 4] += 1;
    repair_crc(&mut data[start..start + size]);
    let mut decoder = Decoder::new(Cursor::new(data)).unwrap();
    assert!(decoder.seek(7500).is_err());
}

#[test]
fn metadata_lengths_duplicates_and_allocation_limits_fail_cleanly() {
    let (start, size) = block(STEREO, 3);
    let mut data = STEREO.to_vec();
    data[start] += 1;
    repair_crc(&mut data[start..start + size]);
    assert!(Decoder::new_stream(Cursor::new(data)).is_err());
    let (start, size) = block(STEREO, 6);
    let mut data = STEREO.to_vec();
    data.splice(
        start - 4..start - 4,
        STEREO[start - 4..start + size].iter().copied(),
    );
    assert!(Decoder::new_stream(Cursor::new(data)).is_err());
    let mut too_many = b"tBaK".to_vec();
    for _ in 0..1025 {
        too_many.extend_from_slice(&[5, 0, 0, 0]);
    }
    assert!(matches!(
        Decoder::new_stream(Cursor::new(too_many)),
        Err(Error::ResourceLimit)
    ));
}

#[test]
fn resync_handles_chunk_boundaries_and_enforces_its_scan_budget() {
    let original = without(STEREO, &[7]);
    let frames = headers(&original);
    let first = frames[0];
    let second = frames[1];
    // The next valid header straddles the 64-KiB scan buffer boundary. The
    // overlap must preserve it even on a short-reading sequential source.
    let mut data = original[..first.offset].to_vec();
    data.resize(first.offset + 65535, 0);
    data.extend_from_slice(&original[second.offset..]);
    let mut decoder =
        Decoder::stream_with_options(Pipe::new(data, 37), options(RecoveryMode::Silence)).unwrap();
    let mut expected = pcm(STEREO_PCM, 16);
    expected[..frame_samples(STEREO) as usize * 2].fill(0);
    assert_eq!(all(&mut decoder).unwrap(), expected);

    let mut data = original[..first.offset].to_vec();
    data.resize(first.offset + 16 * 1024 * 1024 + 1024, 0);
    data.extend_from_slice(&original[second.offset..]);
    let mut decoder =
        Decoder::stream_with_options(Cursor::new(data), options(RecoveryMode::Silence)).unwrap();
    assert!(matches!(
        decoder.read_samples(&mut [0; 2]),
        Err(Error::ResourceLimit)
    ));
    assert!(decoder.damage().is_empty());
}

#[test]
fn damaged_final_header_at_open_does_not_swallow_callbacks_or_format_changes() {
    let frames = headers(STEREO);
    let last = *frames.last().unwrap();
    let mut data = STEREO.to_vec();
    data[last.offset + last.size - 1] ^= 1;
    assert!(Decoder::new(Cursor::new(&data)).is_err());
    let mut decoder =
        Decoder::with_options(Cursor::new(&data), options(RecoveryMode::Silence)).unwrap();
    all(&mut decoder).unwrap();
    assert_eq!(decoder.verification(), Verification::Incomplete);
    assert_eq!(decoder.damage().len(), 1);

    struct TailFailure(Cursor<Vec<u8>>, u64);
    impl Read for TailFailure {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.0.position() >= self.1 {
                return Err(io::Error::other("cancelled at final header"));
            }
            let size = output.len().min((self.1 - self.0.position()) as usize);
            self.0.read(&mut output[..size])
        }
    }
    impl Seek for TailFailure {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.0.seek(position)
        }
    }
    assert!(matches!(
        Decoder::with_options(
            TailFailure(Cursor::new(STEREO.to_vec()), last.offset as u64),
            options(RecoveryMode::Silence)
        ),
        Err(Error::Io(_))
    ));
}

#[test]
fn legacy_seek_table_recovery_skips_a_damaged_pointed_header() {
    let mut data = include_bytes!("fixtures/legacy-seektable.tak").to_vec();
    let audio = block(&data, 0).0;
    let (start, _) = block(&data, 2);
    let entry = &data[start + 4..start + 9];
    let offset = audio
        + u64::from_le_bytes([entry[0], entry[1], entry[2], entry[3], entry[4], 0, 0, 0]) as usize;
    let pointed = header(&data, offset).unwrap();
    data[offset + pointed.size - 1] ^= 1;
    let mut decoder =
        Decoder::with_options(Cursor::new(&data), options(RecoveryMode::Silence)).unwrap();
    decoder.seek(7507).unwrap();
    assert!(all(&mut decoder).unwrap().iter().all(|&n| n == 0));
    assert_eq!(
        decoder.damage(),
        [Damage {
            start: 7507,
            samples: 743
        }]
    );
}

#[test]
fn extensible_pcm_layouts_valid_bits_wave_chunks_and_frame_boundaries() {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extended");
    let cases: [(&str, u16, u16, u32, &[u8]); 7] = [
        ("valid20-stereo", 24, 20, 0x3, &[1, 2]),
        ("quad-back", 16, 16, 0x33, &[1, 2, 5, 6]),
        ("surround51-side", 24, 24, 0x60f, &[1, 2, 3, 4, 10, 11]),
        ("boundary1", 16, 16, 0x4, &[3]),
        ("boundary511", 16, 16, 0x4, &[3]),
        ("boundary512", 16, 16, 0x4, &[3]),
        ("boundary513", 16, 16, 0x4, &[3]),
    ];
    for (name, bits, valid_bits, mask, positions) in cases {
        let data = std::fs::read(directory.join(format!("{name}.tak"))).unwrap();
        let reference = pcm(
            &std::fs::read(directory.join(format!("{name}.s32le"))).unwrap(),
            bits,
        );
        let mut decoder = Decoder::new(Cursor::new(&data)).unwrap();
        assert_eq!(decoder.valid_bits(), valid_bits, "{name}: valid PCM bits");
        assert_eq!(
            &decoder.info().channel_layout[..positions.len()],
            positions,
            "{name}: speaker assignment"
        );
        let wave = decoder.metadata().wave.as_ref().unwrap();
        assert_eq!(wave.head.len(), 68);
        assert_eq!(&wave.head[..4], b"RIFF");
        assert_eq!(&wave.head[38..40], &valid_bits.to_le_bytes());
        assert_eq!(&wave.head[40..44], &mask.to_le_bytes());
        assert_eq!(&wave.head[60..64], b"data");
        assert_eq!(wave.tail, b"LIST\x18\0\0\0INFOISFT\x0c\0\0\0TAK fixture\0");
        let frames = decoder.info().total_samples;
        assert_eq!(all(&mut decoder).unwrap(), reference, "{name}: PCM");
        assert_eq!(
            decoder.verification(),
            Verification::Verified,
            "{name}: canonical PCM MD5"
        );
        for at in [0, 1, 511, 512, 513, frames - 1, frames, frames + 1] {
            decoder.seek(at).unwrap();
            let expected = &reference[at.min(frames) as usize * positions.len()..];
            assert_eq!(all(&mut decoder).unwrap(), expected, "{name}: seek {at}");
        }
        let mut decoder = Decoder::new_stream(Pipe::new(unknown_length(&data), 3)).unwrap();
        assert!(!decoder.length_known());
        assert_eq!(all(&mut decoder).unwrap(), reference);
        assert_eq!(decoder.info().total_samples, frames);
        assert_eq!(decoder.verification(), Verification::Verified);
    }
}

#[test]
fn a_crc_valid_full_frame_without_the_final_flag_is_never_clean_eof() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extended/boundary512.tak");
    let mut data = without(&std::fs::read(path).unwrap(), &[7]);
    let frame = headers(&data)[0];
    assert!(frame.last);
    assert_eq!(frame.samples, 512);
    let mut changed = data[frame.offset..frame.offset + frame.size].to_vec();
    changed[2] &= !1;
    changed.drain(5..7); // The final sample-count field is absent in a normal frame.
    repair_crc(&mut changed);
    data.splice(frame.offset..frame.offset + frame.size, changed);
    assert!(!headers(&data)[0].last);
    for input in [data.clone(), unknown_length(&data)] {
        let mut decoder = Decoder::new_stream(Cursor::new(input)).unwrap();
        assert!(all(&mut decoder).is_err());
        assert_ne!(decoder.verification(), Verification::Verified);
    }
}
