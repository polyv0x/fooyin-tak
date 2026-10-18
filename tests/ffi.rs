// SPDX-License-Identifier: GPL-3.0-or-later
use fooyin_tak::{ffi::*, Damage, StreamInfo, Verification};
use std::ffi::{c_char, c_void, CStr};
use std::io::{Cursor, Read, Seek, SeekFrom};

struct Input {
    data: Cursor<Vec<u8>>,
    fail: bool,
    fail_seek: bool,
    max_read: usize,
}

impl Input {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: Cursor::new(data),
            fail: false,
            fail_seek: false,
            max_read: usize::MAX,
        }
    }

    fn callbacks(&mut self, seekable: bool) -> TakIo {
        TakIo {
            context: (self as *mut Input).cast(),
            read: Some(read),
            seek: if seekable { Some(seek) } else { None },
        }
    }
}

unsafe extern "C" fn read(context: *mut c_void, output: *mut u8, size: usize) -> i64 {
    // SAFETY: the test keeps Input and the destination alive for the callback.
    let input = unsafe { &mut *context.cast::<Input>() };
    if input.fail {
        return -1;
    }
    let buffer = unsafe { std::slice::from_raw_parts_mut(output, size.min(input.max_read)) };
    input.data.read(buffer).map_or(-1, |n| n as i64)
}
unsafe extern "C" fn seek(context: *mut c_void, offset: i64, origin: i32) -> i64 {
    let input = unsafe { &mut *context.cast::<Input>() };
    if input.fail_seek {
        // Failed callbacks may have moved: recovery must not trust the old cursor.
        input.data.set_position(17);
        return -1;
    }
    let position = match origin {
        0 if offset >= 0 => SeekFrom::Start(offset as u64),
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        _ => return -1,
    };
    input.data.seek(position).map_or(-1, |n| n as i64)
}

#[test]
fn ffi_pcm_layout_error_latching_and_recovery() {
    let mut input = Input::new(include_bytes!("fixtures/stereo16.tak").to_vec());
    let io = TakIo {
        context: (&mut input as *mut Input).cast(),
        read: Some(read),
        seek: Some(seek),
    };
    let mut info = StreamInfo::default();
    let mut error = [0 as c_char; 256];
    // SAFETY: all storage is live and exclusively accessed until tak_close.
    unsafe {
        let handle = tak_open(io, &mut info, error.as_mut_ptr(), error.len());
        assert!(
            !handle.is_null(),
            "{}",
            CStr::from_ptr(error.as_ptr()).to_string_lossy()
        );
        assert_eq!(
            (
                info.sample_rate,
                info.channels,
                info.bits_per_sample,
                info.total_samples
            ),
            (8000, 2, 16, 16752)
        );
        let mut pcm = [0i32; 74];
        assert_eq!(tak_read(handle, pcm.as_mut_ptr(), 1), -1);
        assert!(CStr::from_ptr(tak_last_error(handle))
            .to_string_lossy()
            .contains("sample frame"));
        assert_eq!(tak_read(handle, pcm.as_mut_ptr(), pcm.len()), -1);
        assert_eq!(tak_seek(handle, 0), 0);
        assert_eq!(tak_read(handle, pcm.as_mut_ptr(), pcm.len()), 74);
        let expected = include_bytes!("fixtures/stereo16.s32le");
        for (sample, bytes) in pcm.iter().zip(expected.chunks_exact(4)) {
            assert_eq!(*sample, i32::from_le_bytes(bytes.try_into().unwrap()));
        }
        assert_eq!(tak_position(handle), 37);
        assert_eq!(tak_seek(handle, u64::MAX), 0);
        assert_eq!(tak_read(handle, pcm.as_mut_ptr(), pcm.len()), 0);
        tak_close(handle);
        input.fail = true;
        assert!(tak_open(io, &mut info, error.as_mut_ptr(), error.len()).is_null());
        assert!(CStr::from_ptr(error.as_ptr())
            .to_string_lossy()
            .contains("input read failed"));
    }
}

#[test]
fn null_arguments_and_short_error_buffer() {
    unsafe {
        let mut message = [1 as c_char; 1];
        let io = TakIo {
            context: std::ptr::null_mut(),
            read: None,
            seek: None,
        };
        assert!(tak_open(io, std::ptr::null_mut(), message.as_mut_ptr(), 1).is_null());
        assert_eq!(message[0], 0);
        assert_eq!(tak_read(std::ptr::null_mut(), std::ptr::null_mut(), 0), -1);
        assert_eq!(tak_seek(std::ptr::null_mut(), 0), -1);
        assert_eq!(tak_position(std::ptr::null()), 0);
        assert_eq!(tak_is_seekable(std::ptr::null()), -1);
        assert_eq!(tak_sample_count_known(std::ptr::null()), -1);
        assert_eq!(tak_codec(std::ptr::null()), -1);
        assert_eq!(tak_valid_bits(std::ptr::null()), -1);
        assert_eq!(tak_verification(std::ptr::null()), -1);
        assert_eq!(tak_info(std::ptr::null(), std::ptr::null_mut()), -1);
        assert_eq!(tak_encoder_info(std::ptr::null(), std::ptr::null_mut()), -1);
        assert_eq!(tak_expected_md5(std::ptr::null(), std::ptr::null_mut()), -1);
        assert_eq!(
            tak_wave_data(
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            -1
        );
        assert_eq!(tak_damage_count(std::ptr::null()), 0);
        assert_eq!(
            tak_damage_info(std::ptr::null(), 0, std::ptr::null_mut()),
            -1
        );
        tak_close(std::ptr::null_mut());
    }
}

// Keep the context boxed: its address is retained by the C callback interface.
struct Handle {
    handle: *mut TakHandle,
    input: Box<Input>,
    info: StreamInfo,
}

impl Handle {
    fn open(data: Vec<u8>, seekable: bool, options: TakOptions) -> Self {
        let mut input = Box::new(Input::new(data));
        input.max_read = 7; // Exercise callback short reads for metadata and frames.
        let mut info = StreamInfo::default();
        let mut error = [0 as c_char; 256];
        let handle = unsafe {
            tak_open_with_options(
                input.callbacks(seekable),
                &options,
                &mut info,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert!(
            !handle.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Self {
            handle,
            input,
            info,
        }
    }

    fn drain(&mut self) -> Result<Vec<i32>, String> {
        let mut pcm = Vec::new();
        let mut output = [0; 127]; // Partial trailing stereo capacity is ignored.
        loop {
            let count = unsafe { tak_read(self.handle, output.as_mut_ptr(), output.len()) };
            if count < 0 {
                return Err(unsafe { CStr::from_ptr(tak_last_error(self.handle)) }
                    .to_string_lossy()
                    .into_owned());
            }
            if count == 0 {
                return Ok(pcm);
            }
            pcm.extend_from_slice(&output[..count as usize]);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { tak_close(self.handle) };
    }
}

fn reference_pcm() -> Vec<i32> {
    include_bytes!("fixtures/stereo16.s32le")
        .chunks_exact(4)
        .map(|bytes| i32::from_le_bytes(bytes.try_into().unwrap()))
        .collect()
}

fn block(data: &[u8], wanted: u8) -> std::ops::Range<usize> {
    let mut position = 4;
    loop {
        let kind = data[position] & 127;
        let size = u32::from_le_bytes([
            data[position + 1],
            data[position + 2],
            data[position + 3],
            0,
        ]) as usize;
        if kind == wanted {
            return position + 4..position + 4 + size;
        }
        assert_ne!(kind, 0, "missing metadata block {wanted}");
        position += 4 + size;
    }
}

fn rewrite_crc(data: &mut [u8]) {
    let size = data.len() - 3;
    let mut crc = 0xb704ceu32;
    for &byte in &data[..size] {
        crc ^= u32::from(byte) << 16;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x800000 != 0 { 0x864cfb } else { 0 };
        }
        crc &= 0xffffff;
    }
    data[size..].copy_from_slice(&crc.to_le_bytes()[..3]);
}

#[test]
fn sequential_pcm_md5_and_metadata_are_available_through_c() {
    let mut decoder = Handle::open(
        include_bytes!("fixtures/stereo16.tak").to_vec(),
        false,
        TakOptions::default(),
    );
    unsafe {
        assert_eq!(tak_is_seekable(decoder.handle), 0);
        assert_eq!(tak_sample_count_known(decoder.handle), 1);
        assert_eq!(tak_codec(decoder.handle), 2);
        assert_eq!(tak_valid_bits(decoder.handle), 16);
        assert_eq!(
            tak_verification(decoder.handle),
            Verification::Pending as i32
        );
        assert_eq!(tak_read(decoder.handle, std::ptr::null_mut(), 0), 0);
        let mut encoder = TakEncoderInfo::default();
        assert_eq!(tak_encoder_info(decoder.handle, &mut encoder), 1);
        assert_eq!(
            encoder,
            TakEncoderInfo {
                version: 0x020303,
                preset: 4,
                evaluation: 2
            }
        );
        let mut md5 = [0; 16];
        assert_eq!(tak_expected_md5(decoder.handle, md5.as_mut_ptr()), 1);
        assert_eq!(
            md5,
            [
                0xfc, 0xce, 0x7f, 0x55, 0x2c, 0xad, 0x76, 0x79, 0x49, 0xbb, 0xcc, 0xcd, 0xea, 0x20,
                0xa5, 0x6d
            ]
        );
        let mut data = std::ptr::null();
        let mut size = 0;
        assert_eq!(tak_wave_data(decoder.handle, 0, &mut data, &mut size), 1);
        assert_eq!(size, 44);
        assert_eq!(&std::slice::from_raw_parts(data, size)[..4], b"RIFF");
        let header = data; // Borrow remains valid through reads and seeks.
        assert_eq!(tak_wave_data(decoder.handle, 1, &mut data, &mut size), 1);
        assert!(data.is_null());
        assert_eq!(size, 0);
        assert_eq!(tak_wave_data(decoder.handle, 2, &mut data, &mut size), -1);
        assert_eq!(decoder.drain().unwrap(), reference_pcm());
        assert_eq!(
            tak_verification(decoder.handle),
            Verification::Verified as i32
        );
        assert_eq!(std::slice::from_raw_parts(header, 4), b"RIFF");
        assert_eq!(tak_info(decoder.handle, &mut decoder.info), 0);
        assert_eq!(tak_position(decoder.handle), decoder.info.total_samples);
        assert_eq!(tak_damage_count(decoder.handle), 0);
        assert_eq!(
            tak_damage_info(decoder.handle, 0, &mut Damage::default()),
            0
        );
        assert_eq!(tak_seek(decoder.handle, 0), -1);
        assert!(CStr::from_ptr(tak_last_error(decoder.handle))
            .to_string_lossy()
            .contains("not seekable"));
    }
}

#[test]
fn unknown_pipe_length_updates_and_verifies_without_a_seek() {
    let mut data = include_bytes!("fixtures/stereo16.tak").to_vec();
    let stream = block(&data, 1);
    for bit in 14..49 {
        data[stream.start + bit / 8] &= !(1 << (bit % 8));
    }
    rewrite_crc(&mut data[stream]);
    let mut decoder = Handle::open(data, false, TakOptions::default());
    unsafe {
        assert_eq!(tak_sample_count_known(decoder.handle), 0);
        assert_eq!(decoder.info.total_samples, 0);
        assert_eq!(decoder.drain().unwrap(), reference_pcm());
        assert_eq!(tak_info(decoder.handle, &mut decoder.info), 0);
        assert_eq!(decoder.info.total_samples, 16752);
        assert_eq!(tak_sample_count_known(decoder.handle), 1);
        assert_eq!(
            tak_verification(decoder.handle),
            Verification::Verified as i32
        );
    }
}

#[test]
fn md5_mismatch_disabled_and_seek_verification_are_explicit() {
    let mut data = include_bytes!("fixtures/stereo16.tak").to_vec();
    let digest = block(&data, 6);
    data[digest.start] ^= 1;
    rewrite_crc(&mut data[digest]);
    let mut mismatch = Handle::open(data.clone(), true, TakOptions::default());
    assert!(mismatch.drain().unwrap_err().contains("MD5 mismatch"));
    unsafe {
        assert_eq!(
            tak_verification(mismatch.handle),
            Verification::Mismatch as i32
        );
        assert_eq!(tak_seek(mismatch.handle, 100), 0);
        assert_eq!(
            tak_verification(mismatch.handle),
            Verification::Incomplete as i32
        );
        assert_eq!(mismatch.drain().unwrap(), reference_pcm()[200..]);
        assert_eq!(tak_seek(mismatch.handle, 0), 0);
        assert_eq!(
            tak_verification(mismatch.handle),
            Verification::Pending as i32
        );
        assert!(mismatch.drain().unwrap_err().contains("MD5 mismatch"));
    }
    let mut disabled = Handle::open(
        data,
        true,
        TakOptions {
            verify_md5: 0,
            recovery: 0,
        },
    );
    assert_eq!(disabled.drain().unwrap(), reference_pcm());
    assert_eq!(
        unsafe { tak_verification(disabled.handle) },
        Verification::Disabled as i32
    );
}

#[test]
fn callback_failures_are_not_concealed_by_recovery() {
    for recovery in [1, 2] {
        let mut decoder = Handle::open(
            include_bytes!("fixtures/stereo16.tak").to_vec(),
            false,
            TakOptions {
                verify_md5: 1,
                recovery,
            },
        );
        decoder.input.fail = true;
        assert!(decoder.drain().unwrap_err().contains("input read failed"));
        assert_eq!(unsafe { tak_damage_count(decoder.handle) }, 0);
    }
    let mut decoder = Handle::open(
        include_bytes!("fixtures/stereo16.tak").to_vec(),
        true,
        TakOptions::default(),
    );
    decoder.input.fail_seek = true;
    unsafe {
        // The final frame has an indexed offset, so this requires an actual seek.
        assert_eq!(tak_seek(decoder.handle, decoder.info.total_samples - 1), -1);
        assert!(CStr::from_ptr(tak_last_error(decoder.handle))
            .to_string_lossy()
            .contains("input seek failed"));
        decoder.input.fail_seek = false;
        assert_eq!(tak_seek(decoder.handle, 0), 0);
        assert_eq!(decoder.drain().unwrap(), reference_pcm());
    }
}

#[test]
fn recovery_preserves_timeline_and_reports_ranges() {
    let mut data = include_bytes!("fixtures/stereo16.tak").to_vec();
    let start = block(&data, 0).start;
    data[start] = 0; // Damage the first frame's sync, leave later valid frames intact.
    let mut strict = Handle::open(data.clone(), true, TakOptions::default());
    assert!(strict.drain().is_err());
    for seekable in [false, true] {
        for recovery in [1, 2] {
            let mut decoder = Handle::open(
                data.clone(),
                seekable,
                TakOptions {
                    verify_md5: 1,
                    recovery,
                },
            );
            let decoded = decoder.drain().unwrap();
            let mut damage = Damage::default();
            unsafe {
                assert_eq!(tak_damage_count(decoder.handle), 1);
                assert_eq!(tak_damage_info(decoder.handle, 0, &mut damage), 1);
                assert_eq!(
                    tak_verification(decoder.handle),
                    Verification::Incomplete as i32
                );
                assert_eq!(tak_position(decoder.handle), decoder.info.total_samples);
            }
            assert_eq!(damage.start, 0);
            assert!(damage.samples > 0 && damage.samples < decoder.info.total_samples);
            let damaged_samples = damage.samples as usize * usize::from(decoder.info.channels);
            let reference = reference_pcm();
            if recovery == 1 {
                assert_eq!(decoded.len(), reference.len());
                assert!(decoded[..damaged_samples].iter().all(|&value| value == 0));
                assert_eq!(decoded[damaged_samples..], reference[damaged_samples..]);
            } else {
                assert_eq!(decoded, reference[damaged_samples..]);
            }
            if seekable {
                unsafe {
                    assert_eq!(tak_seek(decoder.handle, 0), 0);
                    assert_eq!(tak_damage_count(decoder.handle), 0);
                    assert_eq!(tak_damage_info(decoder.handle, 0, &mut damage), 0);
                    assert_eq!(
                        tak_verification(decoder.handle),
                        Verification::Pending as i32
                    );
                }
            }
        }
    }
}

#[test]
fn invalid_options_callbacks_and_buffers_are_rejected() {
    let mut input = Input::new(include_bytes!("fixtures/stereo16.tak").to_vec());
    let mut info = StreamInfo {
        sample_rate: 123,
        ..StreamInfo::default()
    };
    let mut error = [0 as c_char; 32];
    unsafe {
        for options in [
            TakOptions {
                verify_md5: 2,
                recovery: 0,
            },
            TakOptions {
                verify_md5: 1,
                recovery: 3,
            },
        ] {
            assert!(tak_open_with_options(
                input.callbacks(true),
                &options,
                &mut info,
                error.as_mut_ptr(),
                error.len()
            )
            .is_null());
            assert_eq!(info.sample_rate, 123);
            assert!(CStr::from_ptr(error.as_ptr())
                .to_string_lossy()
                .contains("Invalid TAK"));
            assert_eq!(input.data.position(), 0);
        }
        unsafe extern "C" fn excessive_read(_: *mut c_void, _: *mut u8, size: usize) -> i64 {
            size as i64 + 1
        }
        let bad = TakIo {
            context: std::ptr::null_mut(),
            read: Some(excessive_read),
            seek: None,
        };
        assert!(tak_open(bad, &mut info, std::ptr::null_mut(), usize::MAX).is_null());
        let decoder = Handle::open(
            include_bytes!("fixtures/stereo16.tak").to_vec(),
            true,
            TakOptions::default(),
        );
        assert_eq!(tak_read(decoder.handle, std::ptr::null_mut(), 1), -1);
        assert_eq!(tak_seek(decoder.handle, 0), 0);
        let mut sample = 0;
        assert_eq!(tak_read(decoder.handle, &mut sample, usize::MAX), -1);
        assert_eq!(tak_info(decoder.handle, std::ptr::null_mut()), -1);
        assert_eq!(tak_encoder_info(decoder.handle, std::ptr::null_mut()), -1);
        assert_eq!(tak_expected_md5(decoder.handle, std::ptr::null_mut()), -1);
    }
}

#[test]
fn absent_optional_metadata_and_legacy_encoder_defaults_are_distinct() {
    let original = include_bytes!("fixtures/stereo16.tak");
    let mut stripped = original[..4].to_vec();
    let mut position = 4;
    loop {
        let kind = original[position] & 127;
        let size = u32::from_le_bytes([
            original[position + 1],
            original[position + 2],
            original[position + 3],
            0,
        ]) as usize;
        if !matches!(kind, 3 | 4 | 6) {
            stripped.extend_from_slice(&original[position..position + 4 + size]);
        }
        position += 4 + size;
        if kind == 0 {
            break;
        }
    }
    stripped.extend_from_slice(&original[position..]);
    let mut modern = Handle::open(stripped, true, TakOptions::default());
    unsafe {
        assert_eq!(
            tak_verification(modern.handle),
            Verification::NotPresent as i32
        );
        let mut encoder = TakEncoderInfo {
            version: 123,
            ..TakEncoderInfo::default()
        };
        assert_eq!(tak_encoder_info(modern.handle, &mut encoder), 0);
        assert_eq!(encoder.version, 123);
        let mut digest = [0x42; 16];
        assert_eq!(tak_expected_md5(modern.handle, digest.as_mut_ptr()), 0);
        assert_eq!(digest, [0x42; 16]);
        let mut data = digest.as_ptr();
        let mut size = 123;
        assert_eq!(tak_wave_data(modern.handle, 0, &mut data, &mut size), 0);
        assert!(data.is_null());
        assert_eq!(size, 0);
        assert_eq!(modern.drain().unwrap(), reference_pcm());
        let legacy = Handle::open(
            include_bytes!("fixtures/legacy-stereo16.tak").to_vec(),
            true,
            TakOptions::default(),
        );
        assert_eq!(tak_codec(legacy.handle), 0);
        assert_eq!(tak_encoder_info(legacy.handle, &mut encoder), 1);
        assert_eq!(encoder.version, 0x010000);
        assert_eq!(encoder.evaluation, -1);
    }
}
