// SPDX-License-Identifier: GPL-3.0-or-later
use fooyin_tak::{ffi::*, StreamInfo};
use std::ffi::{c_void, CStr};
use std::io::{Cursor, Read, Seek, SeekFrom};

struct Input {
    data: Cursor<Vec<u8>>,
    fail: bool,
}

unsafe extern "C" fn read(context: *mut c_void, output: *mut u8, size: usize) -> i64 {
    // SAFETY: the test keeps Input and the destination alive for the callback.
    let input = unsafe { &mut *context.cast::<Input>() };
    if input.fail {
        return -1;
    }
    let buffer = unsafe { std::slice::from_raw_parts_mut(output, size) };
    input.data.read(buffer).map_or(-1, |n| n as i64)
}
unsafe extern "C" fn seek(context: *mut c_void, offset: i64, origin: i32) -> i64 {
    let input = unsafe { &mut *context.cast::<Input>() };
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
    let mut input = Input {
        data: Cursor::new(include_bytes!("fixtures/stereo16.tak").to_vec()),
        fail: false,
    };
    let io = TakIo {
        context: (&mut input as *mut Input).cast(),
        read: Some(read),
        seek: Some(seek),
    };
    let mut info = StreamInfo::default();
    let mut error = [0i8; 256];
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
        let mut message = [1i8; 1];
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
        tak_close(std::ptr::null_mut());
    }
}
