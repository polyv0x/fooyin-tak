// SPDX-License-Identifier: GPL-3.0-or-later
//! C ABI used by the Qt adapter. See include/fooyin_tak.h for ownership rules.
//! All decoder work stays on the calling thread; callbacks must not unwind.

use crate::{Damage, Decoder, DecoderOptions, RecoveryMode, StreamInfo};
use std::ffi::{c_char, c_void, CString};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::size_of;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TakIo {
    pub context: *mut c_void,
    pub read: Option<unsafe extern "C" fn(*mut c_void, *mut u8, usize) -> i64>,
    pub seek: Option<unsafe extern "C" fn(*mut c_void, i64, i32) -> i64>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TakOptions {
    pub verify_md5: u32,
    pub recovery: u32,
}

impl Default for TakOptions {
    fn default() -> Self {
        Self {
            verify_md5: 1,
            recovery: 0,
        }
    }
}

impl TakOptions {
    fn decoder_options(self) -> Result<DecoderOptions, &'static str> {
        if self.verify_md5 > 1 {
            return Err("Invalid TAK verification option");
        }
        let recovery = match self.recovery {
            0 => RecoveryMode::Strict,
            1 => RecoveryMode::Silence,
            2 => RecoveryMode::Skip,
            _ => return Err("Invalid TAK recovery option"),
        };
        Ok(DecoderOptions {
            verify_md5: self.verify_md5 != 0,
            recovery,
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TakEncoderInfo {
    pub version: u32,
    pub preset: u32,
    pub evaluation: i32,
}

struct CallbackInput(TakIo);

impl Read for CallbackInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let callback = self
            .0
            .read
            .ok_or_else(|| io::Error::other("missing input read callback"))?;
        // SAFETY: the caller guarantees callback/context validity until close.
        let count = unsafe { callback(self.0.context, output.as_mut_ptr(), output.len()) };
        if count < 0 || count as u64 > output.len() as u64 {
            return Err(io::Error::other("input read failed"));
        }
        Ok(count as usize)
    }
}

impl Seek for CallbackInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let callback = self
            .0
            .seek
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "input is not seekable"))?;
        let (offset, origin) = match position {
            SeekFrom::Start(value) => (
                i64::try_from(value).map_err(|_| io::Error::other("seek offset too large"))?,
                0,
            ),
            SeekFrom::Current(value) => (value, 1),
            SeekFrom::End(value) => (value, 2),
        };
        // SAFETY: same callback lifetime contract as read.
        let result = unsafe { callback(self.0.context, offset, origin) };
        if result < 0 {
            return Err(io::Error::other("input seek failed"));
        }
        Ok(result as u64)
    }
}

pub struct TakHandle {
    decoder: Decoder<CallbackInput>,
    error: CString,
}

fn error_string(message: &str) -> CString {
    CString::new(message.replace('\0', "?")).expect("NULs removed")
}

unsafe fn write_error(buffer: *mut c_char, capacity: usize, message: &str) {
    if !buffer.is_null() && capacity > 0 {
        let count = message.len().min(capacity - 1);
        // SAFETY: caller provides capacity writable bytes, disjoint from message.
        unsafe {
            ptr::copy_nonoverlapping(message.as_ptr(), buffer.cast(), count);
            *buffer.add(count) = 0;
        }
    }
}

/// Open with strict decoding and stored PCM MD5 verification when available.
/// A missing seek callback selects sequential input. Returns null on failure.
///
/// # Safety
/// Callbacks and context must remain valid until tak_close, must not unwind or
/// reenter this handle, and must obey the read/seek contract in the C header.
/// Info must be writable. A nonnull error buffer must hold error_capacity bytes.
#[no_mangle]
pub unsafe extern "C" fn tak_open(
    io: TakIo,
    info: *mut StreamInfo,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut TakHandle {
    // SAFETY: same pointer and callback contract as tak_open_with_options.
    unsafe { tak_open_with_options(io, ptr::null(), info, error, error_capacity) }
}

/// Open with optional explicit settings; null options use the defaults.
///
/// # Safety
/// Same contract as tak_open. Nonnull options must point to readable TakOptions.
/// All output buffers must be disjoint from one another and from input storage.
#[no_mangle]
pub unsafe extern "C" fn tak_open_with_options(
    io: TakIo,
    options: *const TakOptions,
    info: *mut StreamInfo,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut TakHandle {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if info.is_null() || io.read.is_none() {
            return Err("Invalid TAK callback arguments".to_owned());
        }
        // SAFETY: the caller provides readable options when nonnull.
        let options = unsafe { options.as_ref() }
            .copied()
            .unwrap_or_default()
            .decoder_options()
            .map_err(str::to_owned)?;
        let decoder = if io.seek.is_some() {
            Decoder::with_options(CallbackInput(io), options)
        } else {
            Decoder::stream_with_options(CallbackInput(io), options)
        }
        .map_err(|error| error.to_string())?;
        // SAFETY: caller provides a writable StreamInfo.
        unsafe { *info = decoder.info() };
        Ok(Box::into_raw(Box::new(TakHandle {
            decoder,
            error: CString::default(),
        })))
    }));
    let message = match result {
        Ok(Ok(handle)) => {
            // SAFETY: caller's error buffer contract.
            unsafe { write_error(error, error_capacity, "") };
            return handle;
        }
        Ok(Err(message)) => message,
        Err(_) => "Rust TAK decoder panicked while opening input".to_owned(),
    };
    // SAFETY: caller's error buffer contract. Info is only written on success.
    unsafe { write_error(error, error_capacity, &message) };
    ptr::null_mut()
}

/// Free a handle; null is accepted. This does not close the caller's source.
///
/// # Safety
/// A nonnull handle must be live, exclusively owned, and closed exactly once.
#[no_mangle]
pub unsafe extern "C" fn tak_close(handle: *mut TakHandle) {
    if !handle.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: ownership is transferred back by the caller.
            unsafe { drop(Box::from_raw(handle)) };
        }));
    }
}

/// Return interleaved, full-scale signed 32-bit PCM; count is channel samples.
/// Returns 0 at EOF, -1 on error. Errors remain latched until a successful seek.
/// Output is unspecified on error, including a final PCM MD5 mismatch.
///
/// # Safety
/// Handle must be live and exclusively accessed. Output must be aligned and
/// writable for capacity i32s, disjoint from the handle, and at most isize::MAX bytes.
#[no_mangle]
pub unsafe extern "C" fn tak_read(
    handle: *mut TakHandle,
    output: *mut i32,
    capacity: usize,
) -> i64 {
    // SAFETY: caller guarantees a valid handle when nonnull.
    let Some(handle) = (unsafe { handle.as_mut() }) else {
        return -1;
    };
    if !handle.error.as_bytes().is_empty() {
        return -1;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        if capacity == 0 {
            return Ok(0);
        }
        if output.is_null() || capacity > isize::MAX as usize / size_of::<i32>() {
            return Err("Invalid TAK output buffer".to_owned());
        }
        // SAFETY: caller guarantees writable, aligned, exclusive output storage.
        let samples = unsafe { std::slice::from_raw_parts_mut(output, capacity) };
        let count = handle
            .decoder
            .read_samples(samples)
            .map_err(|error| error.to_string())?;
        let shift = 32 - handle.decoder.info().bits_per_sample;
        for value in &mut samples[..count] {
            *value = value.wrapping_shl(u32::from(shift));
        }
        Ok(count as i64)
    }));
    match result {
        Ok(Ok(count)) => count,
        Ok(Err(error)) => {
            handle.error = error_string(&error);
            -1
        }
        Err(_) => {
            handle.error = error_string("Rust TAK decoder panicked while decoding");
            -1
        }
    }
}

/// Seek in sample frames, clamping to known EOF. Clear errors only on success.
/// A pipe cannot seek, even to zero. Nonzero seeks make verification incomplete.
///
/// # Safety
/// A nonnull handle must be live and exclusively accessed.
#[no_mangle]
pub unsafe extern "C" fn tak_seek(handle: *mut TakHandle, sample: u64) -> i32 {
    // SAFETY: caller's handle contract.
    let Some(handle) = (unsafe { handle.as_mut() }) else {
        return -1;
    };
    match catch_unwind(AssertUnwindSafe(|| handle.decoder.seek(sample))) {
        Ok(Ok(())) => {
            handle.error = CString::default();
            0
        }
        Ok(Err(error)) => {
            handle.error = error_string(&error.to_string());
            -1
        }
        Err(_) => {
            handle.error = error_string("Rust TAK decoder panicked while seeking");
            -1
        }
    }
}

/// Query stream properties, including a length learned during playback.
/// Returns 0 on success or -1 for null arguments; output is untouched on error.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified. A nonnull output
/// must be writable, aligned, and disjoint from the handle.
#[no_mangle]
pub unsafe extern "C" fn tak_info(handle: *const TakHandle, info: *mut StreamInfo) -> i32 {
    // SAFETY: caller's handle and output contracts.
    let (Some(handle), Some(info)) = (unsafe { handle.as_ref() }, unsafe { info.as_mut() }) else {
        return -1;
    };
    *info = handle.decoder.info();
    0
}

/// Current position in sample frames, or zero for a null handle.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_position(handle: *const TakHandle) -> u64 {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(0, |handle| handle.decoder.position())
}

/// Returns 1 for seekable input, 0 for a pipe, or -1 for a null handle.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_is_seekable(handle: *const TakHandle) -> i32 {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(-1, |handle| i32::from(handle.decoder.is_seekable()))
}

/// Returns 1 for known length, 0 for unknown length, or -1 for a null handle.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_sample_count_known(handle: *const TakHandle) -> i32 {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(-1, |handle| i32::from(handle.decoder.length_known()))
}

/// Returns the codec number, or -1 for a null handle.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_codec(handle: *const TakHandle) -> i32 {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(-1, |handle| handle.decoder.codec() as i32)
}

/// Returns effective valid PCM bits from the stream header, or -1 for null.
/// The PCM container precision remains StreamInfo.bits_per_sample.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_valid_bits(handle: *const TakHandle) -> i32 {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(-1, |handle| i32::from(handle.decoder.valid_bits()))
}

/// Returns a TakVerification value, or -1 for a null handle.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_verification(handle: *const TakHandle) -> i32 {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(-1, |handle| handle.decoder.verification() as i32)
}

/// Copy encoder metadata. Returns 1 if present, 0 if absent, -1 for null args.
/// Output is only written when metadata is present.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified. A nonnull output
/// must be writable, aligned, and disjoint from the handle.
#[no_mangle]
pub unsafe extern "C" fn tak_encoder_info(
    handle: *const TakHandle,
    info: *mut TakEncoderInfo,
) -> i32 {
    // SAFETY: caller's handle and output contracts.
    let (Some(handle), Some(info)) = (unsafe { handle.as_ref() }, unsafe { info.as_mut() }) else {
        return -1;
    };
    let Some(encoder) = &handle.decoder.metadata().encoder else {
        return 0;
    };
    *info = TakEncoderInfo {
        version: encoder.version,
        preset: u32::from(encoder.preset),
        evaluation: encoder.evaluation.map_or(-1, i32::from),
    };
    1
}

/// Copy the stored 16-byte PCM MD5. Returns 1 if present, 0 if absent, -1 for
/// null args. Output is only written when a digest is present.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified. A nonnull output
/// must hold 16 writable bytes disjoint from the handle.
#[no_mangle]
pub unsafe extern "C" fn tak_expected_md5(handle: *const TakHandle, output: *mut u8) -> i32 {
    // SAFETY: caller's handle contract.
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if output.is_null() {
        return -1;
    }
    let Some(md5) = &handle.decoder.metadata().md5 else {
        return 0;
    };
    // SAFETY: caller provides 16 disjoint writable bytes.
    unsafe { ptr::copy_nonoverlapping(md5.as_ptr(), output, md5.len()) };
    1
}

/// Borrow preserved WAV bytes: part 0 = header, 1 = trailer. Returns 1 if the
/// block exists, 0 if absent, -1 for invalid args. Absent/empty data yields null
/// and length zero. Bytes remain valid until close; never free or modify them.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified. Nonnull outputs
/// must be writable, aligned, and disjoint from each other and the handle.
#[no_mangle]
pub unsafe extern "C" fn tak_wave_data(
    handle: *const TakHandle,
    part: u32,
    data: *mut *const u8,
    size: *mut usize,
) -> i32 {
    // SAFETY: caller's handle and output contracts.
    let (Some(handle), Some(data), Some(size)) = (
        unsafe { handle.as_ref() },
        unsafe { data.as_mut() },
        unsafe { size.as_mut() },
    ) else {
        return -1;
    };
    if part > 1 {
        return -1;
    }
    let Some(wave) = &handle.decoder.metadata().wave else {
        *data = ptr::null();
        *size = 0;
        return 0;
    };
    let bytes = if part == 0 { &wave.head } else { &wave.tail };
    *data = if bytes.is_empty() {
        ptr::null()
    } else {
        bytes.as_ptr()
    };
    *size = bytes.len();
    1
}

/// Number of damaged ranges reported so far, or zero for a null handle.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_damage_count(handle: *const TakHandle) -> usize {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(0, |handle| handle.decoder.damage().len())
}

/// Copy a damaged range in sample frames. Returns 1 on success, 0 for an index
/// past the range count, -1 for null args. Output is only written on success.
/// Ranges describe the current traversal. A successful seek to zero clears them.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified. A nonnull output
/// must be writable, aligned, and disjoint from the handle.
#[no_mangle]
pub unsafe extern "C" fn tak_damage_info(
    handle: *const TakHandle,
    index: usize,
    damage: *mut Damage,
) -> i32 {
    // SAFETY: caller's handle and output contracts.
    let (Some(handle), Some(damage)) = (unsafe { handle.as_ref() }, unsafe { damage.as_mut() })
    else {
        return -1;
    };
    let Some(source) = handle.decoder.damage().get(index) else {
        return 0;
    };
    *damage = *source;
    1
}

/// Error text remains valid until the next operation on the handle.
///
/// # Safety
/// A nonnull handle must be live and not concurrently modified.
#[no_mangle]
pub unsafe extern "C" fn tak_last_error(handle: *const TakHandle) -> *const c_char {
    // SAFETY: caller's handle contract.
    unsafe { handle.as_ref() }.map_or(b"Invalid TAK handle\0".as_ptr().cast(), |handle| {
        handle.error.as_ptr()
    })
}
