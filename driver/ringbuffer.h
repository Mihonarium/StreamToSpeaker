/*
 * ringbuffer.h - Single-producer single-consumer byte ring buffer.
 *
 * Producer: the WaveRT consumer DPC that copies frames out of the
 *           Windows audio engine's WaveRT cyclic buffer.
 * Consumer: the IOCTL completion path that drains the ring into the
 *           pending IRPs from the user-mode service.
 *
 * The buffer holds raw PCM bytes (host endian, 16-bit signed stereo
 * interleaved as L,R,L,R...). Producer-overrun policy: drop oldest
 * (overwrite). We prefer dropping to blocking the audio engine.
 *
 * Memory: a single allocation of capacity bytes (a power of 2 to make
 * masking cheap). Head/tail are unwrapped 64-bit counters; we mask
 * at indexing time. This avoids the "is full vs. empty" ambiguity.
 */

#pragma once

#include "driver.h"

struct StreamToSpeakerRingBuffer {
    /* Backing storage. Allocated in RingBufferInit. */
    UCHAR*      Buffer;
    ULONG       Capacity;      /* bytes, power of 2 */
    ULONG       CapacityMask;  /* Capacity - 1      */

    /* Unwrapped counters. Producer writes Tail; consumer reads Head.
     * Both use Interlocked* on the 64-bit values for cross-IRQL safety
     * (we touch them at DISPATCH_LEVEL). The 64-bit width means they
     * never wrap in any realistic uptime. */
    volatile LONG64  Head;     /* bytes consumed                       */
    volatile LONG64  Tail;     /* bytes produced                       */

    /* Producer-side spinlock. The producer is single-threaded (DPC),
     * but on overrun the producer also advances Head. The consumer
     * is single-threaded (IOCTL dispatch DPC), but on cancellation
     * or device stop we may touch this from PASSIVE_LEVEL too.
     * Hold this whenever you mutate Head or Tail. */
    KSPIN_LOCK  Lock;
};

/* Initialize the buffer. Capacity must be a power of two. Returns
 * STATUS_INSUFFICIENT_RESOURCES on alloc failure. */
NTSTATUS RingBufferInit(
    _Out_ StreamToSpeakerRingBuffer* Rb,
    _In_  ULONG                CapacityBytes);

/* Release backing memory. Safe to call on a never-initialized struct
 * provided it was zeroed. */
VOID RingBufferDestroy(_Inout_ StreamToSpeakerRingBuffer* Rb);

/* Producer: append Bytes worth of data, overwriting oldest data if
 * full. Returns count of bytes overwritten (>0 means lossy). */
ULONG RingBufferWrite(
    _Inout_ StreamToSpeakerRingBuffer* Rb,
    _In_reads_bytes_(Bytes) const VOID* Data,
    _In_ ULONG Bytes);

/* Consumer: read up to MaxBytes into Out. Returns bytes actually
 * read. Returns 0 if buffer is empty. */
ULONG RingBufferRead(
    _Inout_ StreamToSpeakerRingBuffer* Rb,
    _Out_writes_bytes_(MaxBytes) VOID* Out,
    _In_ ULONG MaxBytes);

/* Snapshot of current fill in bytes. May race; advisory only. */
ULONG RingBufferAvailable(_In_ const StreamToSpeakerRingBuffer* Rb);

/* Drop all buffered audio. Called on stream stop. */
VOID RingBufferReset(_Inout_ StreamToSpeakerRingBuffer* Rb);
