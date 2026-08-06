/*
 * ringbuffer.cpp - SPSC byte ring buffer implementation.
 *
 * See ringbuffer.h for the contract. Notes:
 *  - Allocation uses NonPagedPoolNx (audio path is at DISPATCH_LEVEL).
 *  - Head/Tail are 64-bit so they never wrap. Indexing masks with
 *    CapacityMask, which assumes Capacity is a power of two.
 *  - The overwrite path advances Head atomically so a concurrent
 *    consumer reads a consistent state.
 */

#include "ringbuffer.h"

NTSTATUS
RingBufferInit(
    _Out_ StreamToSpeakerRingBuffer* Rb,
    _In_  ULONG                CapacityBytes)
{
    if (Rb == nullptr || CapacityBytes == 0) {
        return STATUS_INVALID_PARAMETER;
    }
    /* Require power of two. */
    if ((CapacityBytes & (CapacityBytes - 1)) != 0) {
        return STATUS_INVALID_PARAMETER;
    }

    RtlZeroMemory(Rb, sizeof(*Rb));

    Rb->Buffer = static_cast<UCHAR*>(
        ExAllocatePool2(POOL_FLAG_NON_PAGED, CapacityBytes, STREAM_TO_SPEAKER_POOL_TAG));
    if (Rb->Buffer == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    Rb->Capacity     = CapacityBytes;
    Rb->CapacityMask = CapacityBytes - 1;
    Rb->Head         = 0;
    Rb->Tail         = 0;
    KeInitializeSpinLock(&Rb->Lock);

    return STATUS_SUCCESS;
}

VOID
RingBufferDestroy(_Inout_ StreamToSpeakerRingBuffer* Rb)
{
    if (Rb == nullptr) {
        return;
    }
    if (Rb->Buffer != nullptr) {
        ExFreePoolWithTag(Rb->Buffer, STREAM_TO_SPEAKER_POOL_TAG);
        Rb->Buffer = nullptr;
    }
    Rb->Capacity     = 0;
    Rb->CapacityMask = 0;
    Rb->Head         = 0;
    Rb->Tail         = 0;
}

ULONG
RingBufferAvailable(_In_ const StreamToSpeakerRingBuffer* Rb)
{
    if (Rb == nullptr || Rb->Buffer == nullptr) {
        return 0;
    }
    /* Volatile reads; ordering doesn't matter for an advisory value. */
    LONG64 head = ReadAcquire64(const_cast<volatile LONG64*>(&Rb->Head));
    LONG64 tail = ReadAcquire64(const_cast<volatile LONG64*>(&Rb->Tail));
    LONG64 avail = tail - head;
    if (avail < 0) {
        return 0;
    }
    if (avail > (LONG64)Rb->Capacity) {
        avail = (LONG64)Rb->Capacity;
    }
    return (ULONG)avail;
}

VOID
RingBufferReset(_Inout_ StreamToSpeakerRingBuffer* Rb)
{
    if (Rb == nullptr || Rb->Buffer == nullptr) {
        return;
    }
    KIRQL old;
    KeAcquireSpinLock(&Rb->Lock, &old);
    Rb->Head = Rb->Tail;
    KeReleaseSpinLock(&Rb->Lock, old);
}

ULONG
RingBufferWrite(
    _Inout_ StreamToSpeakerRingBuffer* Rb,
    _In_reads_bytes_(Bytes) const VOID* Data,
    _In_ ULONG Bytes)
{
    if (Rb == nullptr || Rb->Buffer == nullptr || Data == nullptr || Bytes == 0) {
        return 0;
    }

    /* Cap at capacity — writing more than the buffer can hold is
     * inherently lossy; only the tail Bytes are kept. */
    ULONG effective = Bytes;
    ULONG skipSrc = 0;
    if (effective > Rb->Capacity) {
        skipSrc   = effective - Rb->Capacity;
        effective = Rb->Capacity;
    }

    const UCHAR* src = static_cast<const UCHAR*>(Data) + skipSrc;

    ULONG overwritten = skipSrc;

    KIRQL old;
    KeAcquireSpinLock(&Rb->Lock, &old);

    LONG64 head = Rb->Head;
    LONG64 tail = Rb->Tail;
    LONG64 used = tail - head;
    LONG64 free = (LONG64)Rb->Capacity - used;

    if ((LONG64)effective > free) {
        ULONG drop = (ULONG)((LONG64)effective - free);
        Rb->Head    = head + drop;
        head        = Rb->Head;
        overwritten += drop;
    }

    ULONG idx = (ULONG)(tail & Rb->CapacityMask);
    ULONG first = Rb->Capacity - idx;
    if (first > effective) {
        first = effective;
    }
    RtlCopyMemory(Rb->Buffer + idx, src, first);
    ULONG second = effective - first;
    if (second > 0) {
        RtlCopyMemory(Rb->Buffer, src + first, second);
    }
    Rb->Tail = tail + effective;

    UNREFERENCED_PARAMETER(head); /* head reused only for debugging */

    KeReleaseSpinLock(&Rb->Lock, old);

    return overwritten;
}

ULONG
RingBufferRead(
    _Inout_ StreamToSpeakerRingBuffer* Rb,
    _Out_writes_bytes_(MaxBytes) VOID* Out,
    _In_ ULONG MaxBytes)
{
    if (Rb == nullptr || Rb->Buffer == nullptr || Out == nullptr || MaxBytes == 0) {
        return 0;
    }

    KIRQL old;
    KeAcquireSpinLock(&Rb->Lock, &old);

    LONG64 head = Rb->Head;
    LONG64 tail = Rb->Tail;
    LONG64 avail = tail - head;
    if (avail <= 0) {
        KeReleaseSpinLock(&Rb->Lock, old);
        return 0;
    }
    if (avail > (LONG64)Rb->Capacity) {
        /* Should never happen — implies an overflow we didn't track.
         * Be defensive: clamp. */
        avail = (LONG64)Rb->Capacity;
        Rb->Head = tail - avail;
        head = Rb->Head;
    }

    ULONG toCopy = (ULONG)avail;
    if (toCopy > MaxBytes) {
        toCopy = MaxBytes;
    }

    ULONG idx = (ULONG)(head & Rb->CapacityMask);
    ULONG first = Rb->Capacity - idx;
    if (first > toCopy) {
        first = toCopy;
    }
    RtlCopyMemory(Out, Rb->Buffer + idx, first);
    ULONG second = toCopy - first;
    if (second > 0) {
        RtlCopyMemory(static_cast<UCHAR*>(Out) + first, Rb->Buffer, second);
    }
    Rb->Head = head + toCopy;

    KeReleaseSpinLock(&Rb->Lock, old);

    return toCopy;
}
