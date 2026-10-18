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
        uint64_t total_samples;
        uint32_t sample_rate;
        uint16_t channels;
        uint16_t bits_per_sample;
        uint8_t channel_layout[16];
    } TakStreamInfo;

    // The caller owns context and the source. They must outlive the decoder.
    // read: return bytes copied (0 = EOF, negative = error), never more than size.
    // seek: origin 0/1/2 = start/current/end; return absolute position or -1.
    // Callbacks run synchronously and must not throw, unwind, or reenter the handle.
    typedef struct
    {
        void *context;
        int64_t (*read)(void *context, uint8_t *output, size_t size);
        int64_t (*seek)(void *context, int64_t offset, int32_t origin);
    } TakIo;

    TakHandle *tak_open(TakIo io, TakStreamInfo *info, char *error, size_t error_capacity);
    void tak_close(TakHandle *handle);
    // Returns channel samples written, 0 for EOF/zero capacity, or -1 on error.
    // PCM is interleaved native-endian S32, with source samples shifted to full scale.
    // Output must fit one whole sample frame; partial trailing capacity is ignored.
    int64_t tak_read(TakHandle *handle, int32_t *output, size_t capacity);
    // Seeks in sample frames, not milliseconds. Clears errors; clamps to EOF.
    int32_t tak_seek(TakHandle *handle, uint64_t sample);
    uint64_t tak_position(const TakHandle *handle);
    // Borrowed NUL-terminated text, invalidated by any subsequent handle operation.
    const char *tak_last_error(const TakHandle *handle);

    // Each handle requires exclusive access; separate handles may run concurrently.
    // Close each handle exactly once, before releasing its callback context.

#ifdef __cplusplus
}
#endif
