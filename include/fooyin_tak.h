// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C"
{
#endif

    typedef struct TakHandle TakHandle;

    typedef struct
    {
        uint64_t total_samples; // Sample frames; zero means unknown until resolved.
        uint32_t sample_rate;
        uint16_t channels;
        uint16_t bits_per_sample;
        uint8_t channel_layout[16];
    } TakStreamInfo;

    // The caller owns context and the source. They must outlive the decoder.
    // read: return bytes copied (0 = EOF, negative = error), never more than size.
    // seek: optional; origin 0/1/2 = start/current/end; return absolute position or -1.
    // A null seek callback selects sequential input starting at the TAK signature.
    // Callbacks run synchronously and must not throw, unwind, or reenter the handle.
    typedef struct
    {
        void *context;
        int64_t (*read)(void *context, uint8_t *output, size_t size);
        int64_t (*seek)(void *context, int64_t offset, int32_t origin);
    } TakIo;

    typedef enum
    {
        TAK_RECOVERY_STRICT = 0,
        TAK_RECOVERY_SILENCE = 1,
        TAK_RECOVERY_SKIP = 2
    } TakRecovery;

    typedef struct
    {
        uint32_t verify_md5; // 0 = disabled, 1 = check stored whole-PCM MD5.
        uint32_t recovery;   // A TakRecovery value; I/O failures are never concealed.
    } TakOptions;

    typedef enum
    {
        TAK_VERIFICATION_NOT_PRESENT = 0,
        TAK_VERIFICATION_DISABLED = 1,
        TAK_VERIFICATION_PENDING = 2,
        TAK_VERIFICATION_VERIFIED = 3,
        TAK_VERIFICATION_MISMATCH = 4,
        TAK_VERIFICATION_INCOMPLETE = 5
    } TakVerification;

    typedef struct
    {
        uint32_t version; // 0xMMmmpp = major, minor, patch (e.g. 0x020303).
        uint32_t preset;
        int32_t evaluation; // -1 = unknown, 0 = standard, 1 = extra, 2 = maximum.
    } TakEncoderInfo;

    typedef struct
    {
        uint64_t start;   // First damaged sample frame in the original timeline.
        uint64_t samples; // Number of damaged sample frames.
    } TakDamage;

    // Default options: MD5 enabled, strict decoding. info is only written on success.
    // A null error buffer is accepted. Nonempty error buffers are always NUL-terminated.
    TakHandle *tak_open(TakIo io, TakStreamInfo *info, char *error, size_t error_capacity);
    // Null options selects the defaults; invalid option values are rejected.
    TakHandle *tak_open_with_options(TakIo io, const TakOptions *options, TakStreamInfo *info,
                                    char *error, size_t error_capacity);
    // Null is accepted; caller-owned context/source are not freed or closed.
    void tak_close(TakHandle *handle);

    // Returns channel samples written, 0 for EOF/zero capacity, or -1 on error.
    // PCM is interleaved native-endian S32, with source samples shifted to full scale.
    // Output must fit one whole sample frame; partial trailing capacity is ignored.
    // Output is unspecified on error, including a final MD5 mismatch.
    // Errors remain latched until a successful seek. Null output is allowed only
    // with zero capacity. Never pass capacity larger than PTRDIFF_MAX / sizeof(int32_t).
    int64_t tak_read(TakHandle *handle, int32_t *output, size_t capacity);
    // Seeks in sample frames, not milliseconds; clamps to known EOF. Returns 0 on
    // success and clears errors; -1 on failure. Pipes cannot seek. Seeking to zero
    // restarts MD5; other nontrivial seeks make verification incomplete.
    int32_t tak_seek(TakHandle *handle, uint64_t sample);

    // Returns 0 on success, -1 for null args. Reflects length learned during playback.
    int32_t tak_info(const TakHandle *handle, TakStreamInfo *info);
    uint64_t tak_position(const TakHandle *handle); // Zero for null handle.
    int32_t tak_is_seekable(const TakHandle *handle); // 1 = yes, 0 = no, -1 = null.
    int32_t tak_sample_count_known(const TakHandle *handle); // 1 = yes, 0 = no, -1 = null.
    int32_t tak_codec(const TakHandle *handle); // Codec number, -1 = null.
    // Effective valid PCM bits; container precision remains bits_per_sample.
    int32_t tak_valid_bits(const TakHandle *handle); // -1 = null.
    int32_t tak_verification(const TakHandle *handle); // TakVerification, -1 = null.

    // Metadata copies return 1 if present, 0 if absent, -1 for invalid/null args.
    // Outputs are only written when metadata is present, and must not alias the handle.
    int32_t tak_encoder_info(const TakHandle *handle, TakEncoderInfo *info);
    int32_t tak_expected_md5(const TakHandle *handle, uint8_t output[16]);
    // part: 0 = WAV header, 1 = WAV trailer. Returns 1 if WAV metadata is present,
    // 0 if absent, -1 for invalid args. Absent/empty bytes yield null and size zero.
    // Borrowed bytes remain valid until close; never free or modify them.
    int32_t tak_wave_data(const TakHandle *handle, uint32_t part, const uint8_t **data, size_t *size);
    size_t tak_damage_count(const TakHandle *handle); // Zero for null.
    // Returns 1 if found, 0 if index is out of range, -1 for null args.
    // Ranges are in sample frames for this traversal; seeking to zero clears them.
    int32_t tak_damage_info(const TakHandle *handle, size_t index, TakDamage *damage);

    // Borrowed NUL-terminated text, invalidated by any subsequent handle operation.
    const char *tak_last_error(const TakHandle *handle);

    // Each handle requires exclusive access; separate handles may run concurrently.
    // All nonnull pointers must be valid and aligned, and output buffers must be
    // disjoint from each other, the handle, and callback input storage.
    // Close each handle exactly once, before releasing its callback context.

#ifdef __cplusplus
}
#endif
