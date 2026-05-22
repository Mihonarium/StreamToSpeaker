/*
 * newdelete.cpp - Placement new/delete operators for POOL_FLAGS.
 *
 * WDK 10.0.22000+ removed the inline operator new/delete declarations
 * that older versions of stdunk.h provided. Sysvad's NewDelete.cpp
 * shows the standard replacement: a small set of placement-new
 * overloads keyed on POOL_FLAGS (the modern pool allocator argument)
 * that wrap ExAllocatePool2 / ExFreePool.
 *
 * The driver uses `new (POOL_FLAGS, ULONG_TAG) Class(...)` to allocate
 * C++ objects from non-paged pool, mirroring sysvad's idiom.
 */

#include "driver.h"

#ifndef EMULATE_WINDOWS_KERNEL

/* ------------------------------------------------------------------ */
/* operator new (POOL_FLAGS, ULONG tag)                                */
/* ------------------------------------------------------------------ */

PVOID operator new(
    size_t      iSize,
    POOL_FLAGS  poolFlags,
    ULONG       tag)
{
    PVOID result = ExAllocatePool2(poolFlags, iSize, tag);
    /* ExAllocatePool2 zeros memory by default. */
    return result;
}

/* ------------------------------------------------------------------ */
/* operator new (POOL_FLAGS)                                           */
/* ------------------------------------------------------------------ */

PVOID operator new(
    size_t      iSize,
    POOL_FLAGS  poolFlags)
{
    PVOID result = ExAllocatePool2(poolFlags, iSize, STREAM_TO_SPEAKER_POOL_TAG);
    return result;
}

/* Note: operator delete overloads are provided by stdunk.h (inline)
 * on all WDK versions we target. We deliberately do NOT define them
 * here, to avoid clashing with stdunk's inline copies. The placement-
 * new (POOL_FLAGS) overloads above are distinct from stdunk's
 * POOL_TYPE-keyed ones because POOL_TYPE is an enum and POOL_FLAGS is
 * a typedef'd ULONG64. */

#endif // EMULATE_WINDOWS_KERNEL
