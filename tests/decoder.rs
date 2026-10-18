// SPDX-License-Identifier: GPL-3.0-or-later
use fooyin_tak::{Decoder, Error};
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

const NAMES: &[&str] = &[
    "mono8",
    "stereo8",
    "stereo16",
    "surround24",
    "silence16",
    "stereo24",
    "short24",
    "negative16",
    "legacy-mono8",
    "legacy-stereo8",
    "legacy-stereo16",
    "legacy-stereo24",
    "legacy-silence16",
    "legacy-negative24",
];

fn fixture(name: &str, extension: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{name}.{extension}")),
    )
    .unwrap()
}
fn expected(name: &str) -> Vec<i32> {
    fixture(name, "s32le")
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}
fn all<R: Read + Seek>(decoder: &mut Decoder<R>) -> Result<Vec<i32>, Error> {
    let shift = 32 - decoder.info().bits_per_sample;
    let mut samples = vec![0; 317 * usize::from(decoder.info().channels) + 1];
    let mut output = Vec::new();
    loop {
        let count = decoder.read_samples(&mut samples)?;
        if count == 0 {
            return Ok(output);
        }
        output.extend(
            samples[..count]
                .iter()
                .map(|x| x.wrapping_shl(u32::from(shift))),
        );
    }
}

#[test]
fn reference_pcm_for_all_fixtures() {
    for name in NAMES {
        let mut decoder = Decoder::new(Cursor::new(fixture(name, "tak"))).unwrap();
        let pcm = all(&mut decoder).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(pcm == expected(name), "PCM mismatch: {name}");
        assert_eq!(decoder.position(), decoder.info().total_samples);
    }
}

#[test]
fn sample_accurate_seeking_including_unvisited_frames() {
    for name in NAMES {
        let data = fixture(name, "tak");
        let reference = expected(name);
        let mut decoder = Decoder::new(Cursor::new(data.clone())).unwrap();
        let info = decoder.info();
        let channels = usize::from(info.channels);
        for sample in [
            info.total_samples / 2,
            0,
            info.total_samples - 1,
            511,
            512,
            513,
            1023,
            1024,
            1500,
            8192,
            u64::MAX,
            1,
        ] {
            decoder.seek(sample);
            let start = sample.min(info.total_samples) as usize * channels;
            assert!(
                all(&mut decoder).unwrap() == reference[start..],
                "{name}: seek {sample}"
            );
            // Start from a fresh decoder to cover header scans without cached offsets.
            let mut fresh = Decoder::new(Cursor::new(data.clone())).unwrap();
            fresh.seek(sample);
            assert!(
                all(&mut fresh).unwrap() == reference[start..],
                "{name}: fresh seek {sample}"
            );
        }
        decoder.seek_ms(u64::MAX);
        assert_eq!(decoder.position(), info.total_samples);
    }
}

struct ShortReads(Cursor<Vec<u8>>);
impl Read for ShortReads {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = output.len().min(3);
        self.0.read(&mut output[..count])
    }
}
impl Seek for ShortReads {
    fn seek(&mut self, offset: SeekFrom) -> io::Result<u64> {
        self.0.seek(offset)
    }
}

#[test]
fn short_reads_and_trailing_tags() {
    let mut data = fixture("stereo16", "tak");
    data.extend_from_slice(b"APETAGEX\0\0\0\0arbitrary trailing bytes");
    let mut decoder = Decoder::new(ShortReads(Cursor::new(data))).unwrap();
    assert_eq!(all(&mut decoder).unwrap(), expected("stereo16"));
}

#[test]
fn output_requires_complete_sample_frames() {
    let mut decoder = Decoder::new(Cursor::new(fixture("surround24", "tak"))).unwrap();
    assert_eq!(decoder.read_samples(&mut []).unwrap(), 0);
    assert!(decoder.read_samples(&mut [0; 5]).is_err());
    assert_eq!(decoder.position(), 0);
    assert_eq!(decoder.read_samples(&mut [0; 13]).unwrap(), 12);
    assert_eq!(decoder.position(), 2);
    assert_eq!(&decoder.info().channel_layout[..6], &[1, 2, 3, 4, 5, 6]);
}

fn metadata_end(data: &[u8]) -> usize {
    let mut offset = 4;
    while data[offset] & 127 != 0 {
        let size =
            u32::from_le_bytes([data[offset + 1], data[offset + 2], data[offset + 3], 0]) as usize;
        offset += 4 + size;
    }
    offset + 4
}

fn rejects(data: Vec<u8>) -> bool {
    match Decoder::new(Cursor::new(data)) {
        Err(_) => true,
        Ok(mut decoder) => all(&mut decoder).is_err(),
    }
}

#[test]
fn corrupt_headers_and_payloads_never_return_bad_pcm() {
    let data = fixture("stereo16", "tak");
    let mut header = data.clone();
    header[8] ^= 1;
    assert!(Decoder::new(Cursor::new(header)).is_err());
    let start = metadata_end(&data);
    for position in [start, start + 5, start + 25, start + 100, data.len() - 1] {
        let mut corrupt = data.clone();
        corrupt[position] ^= 1;
        assert!(rejects(corrupt), "accepted corruption at {position}");
    }
    // Every possible truncation of a small valid file must fail cleanly.
    let small = fixture("short24", "tak");
    for len in 0..small.len() {
        assert!(
            rejects(small[..len].to_vec()),
            "accepted truncation at {len}"
        );
    }
}

#[test]
fn malformed_residuals_do_not_panic() {
    for name in ["stereo16", "legacy-stereo16", "legacy-stereo24"] {
        let data = fixture(name, "tak");
        let start = metadata_end(&data);
        // Header CRCs stay intact; altered entropy/filter bits reach the decoder.
        // A valid payload may be returned only if it is identical to the reference.
        let reference = expected(name);
        for index in 0..600 {
            let mut corrupt = data.clone();
            let at = start + 24 + (index * 7919) % (data.len() - start - 24);
            corrupt[at] ^= 1 << (index % 8);
            if let Ok(mut decoder) = Decoder::new(Cursor::new(corrupt)) {
                if let Ok(output) = all(&mut decoder) {
                    assert_eq!(output, reference);
                }
            }
        }
    }
}
