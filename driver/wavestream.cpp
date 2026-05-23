/*
 * wavestream.cpp - CMiniportWaveRTStream implementation.
 *
 * Allocates the WaveRT cyclic buffer with non-cached pages, exposes
 * it to the Windows audio engine via AllocateAudioBuffer, and runs a
 * periodic DPC while in KSSTATE_RUN that:
 *
 *   1. Computes how many frames the engine has produced since the
 *      previous tick using elapsed QPC time. (We don't have a real
 *      hardware position register, so we synthesize one from the
 *      sample clock.)
 *   2. Copies those frames out of the cyclic buffer into the IOCTL
 *      ring buffer.
 *   3. Calls IoctlTryCompleteAudio() to wake up any pending IRPs.
 *
 * The DPC fires every STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS (2 ms by
 * default), matching the WaveRT minimum buffer of 2 ms.
 *
 * Position registers: WaveRT supports HW position registers, but a
 * virtual device doesn't need a polled register — we return the
 * software-tracked counter via GetPosition. GetPositionRegister
 * returns no register (Register==NULL), forcing the audio engine to
 * call GetPosition periodically.
 */

#include "wavestream.h"
#include "wave.h"

/* ------------------------------------------------------------------ */
/* DPC thunk                                                           */
/* ------------------------------------------------------------------ */

static KDEFERRED_ROUTINE ConsumerDpcRoutine;

static VOID
ConsumerDpcRoutine(
    _In_     struct _KDPC* Dpc,
    _In_opt_ PVOID         DeferredContext,
    _In_opt_ PVOID         SystemArgument1,
    _In_opt_ PVOID         SystemArgument2)
{
    UNREFERENCED_PARAMETER(Dpc);
    UNREFERENCED_PARAMETER(SystemArgument1);
    UNREFERENCED_PARAMETER(SystemArgument2);
    if (DeferredContext == nullptr) {
        return;
    }
    static_cast<CMiniportWaveRTStream*>(DeferredContext)->OnConsumerDpc();
}

/* ------------------------------------------------------------------ */
/* Construction / IUnknown                                             */
/* ------------------------------------------------------------------ */

STDMETHODIMP
CMiniportWaveRTStream::NonDelegatingQueryInterface(
    _In_ REFIID Interface,
    _COM_Outptr_ PVOID* Object)
{
    if (Object == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    /* IMiniportWaveRTStreamNotification : IMiniportWaveRTStream :
     * IUnknown, so a single chain of casts handles all three IIDs. */
    if (IsEqualGUIDAligned(Interface, IID_IUnknown)) {
        *Object = PVOID(PUNKNOWN(static_cast<IMiniportWaveRTStreamNotification*>(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniportWaveRTStream)) {
        *Object = PVOID(static_cast<IMiniportWaveRTStream*>(
                    static_cast<IMiniportWaveRTStreamNotification*>(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniportWaveRTStreamNotification)) {
        *Object = PVOID(static_cast<IMiniportWaveRTStreamNotification*>(this));
    } else {
        *Object = nullptr;
    }
    if (*Object != nullptr) {
        PUNKNOWN(*Object)->AddRef();
        return STATUS_SUCCESS;
    }
    return STATUS_INVALID_PARAMETER;
}

CMiniportWaveRTStream::~CMiniportWaveRTStream()
{
    StopTimer();
    if (m_BufferMdl != nullptr) {
        /* Memory is freed via PortCls when the stream is torn down,
         * but if we allocated the MDL ourselves (no PortCls helper),
         * release it here. */
        IoFreeMdl(m_BufferMdl);
        m_BufferMdl = nullptr;
    }
    if (m_BufferVa != nullptr) {
        ExFreePoolWithTag(m_BufferVa, STREAM_TO_SPEAKER_POOL_TAG);
        m_BufferVa = nullptr;
    }
    if (m_PortStream != nullptr) {
        m_PortStream->Release();
        m_PortStream = nullptr;
    }
    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext = DeviceExtension();
    if (ext != nullptr && ext->ActiveStream == this) {
        ext->ActiveStream = nullptr;
    }
}

NTSTATUS
CMiniportWaveRTStream::Init(
    _In_ CMiniportWaveRT*  Miniport,
    _In_ PPORTWAVERTSTREAM PortStream,
    _In_ ULONG             Pin,
    _In_ PKSDATAFORMAT     DataFormat)
{
    PAGED_CODE();
    UNREFERENCED_PARAMETER(DataFormat);
    if (Miniport == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    m_Miniport               = Miniport;
    m_PortStream             = PortStream;
    if (m_PortStream != nullptr) {
        m_PortStream->AddRef();
    }
    m_PinId                  = Pin;
    m_Allocated              = FALSE;
    m_State                  = KSSTATE_STOP;
    m_BufferMdl              = nullptr;
    m_BufferVa               = nullptr;
    m_BufferBytes            = 0;
    m_StreamFramesProduced   = 0;
    m_StreamFramesConsumed   = 0;
    m_TimerStarted           = FALSE;
    m_TimerResolutionRaised  = FALSE;
    m_DpcLogCounter          = 0;
    m_NotificationEventCount = 0;
    m_NotificationsPerBuffer = 0;
    m_BytesPerNotification   = 0;
    m_LastNotificationConsumed = 0;
    for (ULONG i = 0; i < STREAM_TO_SPEAKER_MAX_NOTIFICATION_EVENTS; ++i) {
        m_NotificationEvents[i] = nullptr;
    }
    KeInitializeTimer(&m_Timer);
    KeInitializeDpc(&m_TimerDpc, ConsumerDpcRoutine, this);
    KeInitializeSpinLock(&m_StateLock);
    KeInitializeSpinLock(&m_EventLock);

    /* 2 ms in 100-ns units = -20000 (negative means relative). */
    m_TimerInterval.QuadPart =
        -(LONGLONG)STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS * 10000LL;

    LARGE_INTEGER freq;
    m_LastTickQpc  = KeQueryPerformanceCounter(&freq);
    m_PerfFrequency = freq;

    return STATUS_SUCCESS;
}

PSTREAM_TO_SPEAKER_DEVICE_EXTENSION
CMiniportWaveRTStream::DeviceExtension()
{
    return (m_Miniport != nullptr) ? m_Miniport->DeviceExtension() : nullptr;
}

/* ------------------------------------------------------------------ */
/* WaveRT buffer allocation                                            */
/* ------------------------------------------------------------------ */

STDMETHODIMP
CMiniportWaveRTStream::AllocateAudioBuffer(
    _In_  ULONG          RequestedSize,
    _Out_ PMDL*          OutMdl,
    _Out_ ULONG*         OutAllocatedSize,
    _Out_ ULONG*         OutOffset,
    _Out_ MEMORY_CACHING_TYPE* OutCacheType)
{
    PAGED_CODE();
    if (OutMdl == nullptr || OutAllocatedSize == nullptr ||
        OutOffset == nullptr || OutCacheType == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    *OutMdl = nullptr;
    *OutAllocatedSize = 0;
    *OutOffset = 0;
    *OutCacheType = MmCached;

    /* Round up to whole frame and align to a page. Cap at 64 KB so we
     * never allocate something silly. */
    if (RequestedSize == 0) {
        RequestedSize = PAGE_SIZE;
    }
    if (RequestedSize > 0x10000) {
        RequestedSize = 0x10000;
    }
    RequestedSize -= (RequestedSize % STREAM_TO_SPEAKER_FRAME_BYTES);
    if (RequestedSize == 0) {
        return STATUS_INVALID_PARAMETER;
    }

    UCHAR* va = static_cast<UCHAR*>(
        ExAllocatePool2(POOL_FLAG_NON_PAGED, RequestedSize, STREAM_TO_SPEAKER_POOL_TAG));
    if (va == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    PMDL mdl = IoAllocateMdl(va, RequestedSize, FALSE, FALSE, nullptr);
    if (mdl == nullptr) {
        ExFreePoolWithTag(va, STREAM_TO_SPEAKER_POOL_TAG);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    MmBuildMdlForNonPagedPool(mdl);

    m_BufferMdl   = mdl;
    m_BufferVa    = va;
    m_BufferBytes = RequestedSize;
    m_Allocated   = TRUE;
    m_StreamFramesProduced = 0;
    m_StreamFramesConsumed = 0;
    /* Default to a single notification per buffer wrap. Overwritten
     * if the engine uses AllocateBufferWithNotification. */
    if (m_NotificationsPerBuffer == 0) {
        m_NotificationsPerBuffer = 1;
    }
    m_BytesPerNotification = RequestedSize / m_NotificationsPerBuffer;
    m_LastNotificationConsumed = 0;

    *OutMdl           = mdl;
    *OutAllocatedSize = RequestedSize;
    *OutOffset        = 0;
    *OutCacheType     = MmCached;
    return STATUS_SUCCESS;
}

STDMETHODIMP_(VOID)
CMiniportWaveRTStream::FreeAudioBuffer(
    _In_opt_ PMDL Mdl,
    _In_     ULONG Size)
{
    PAGED_CODE();
    UNREFERENCED_PARAMETER(Mdl);
    UNREFERENCED_PARAMETER(Size);
    StopTimer();
    if (m_BufferMdl != nullptr) {
        IoFreeMdl(m_BufferMdl);
        m_BufferMdl = nullptr;
    }
    if (m_BufferVa != nullptr) {
        ExFreePoolWithTag(m_BufferVa, STREAM_TO_SPEAKER_POOL_TAG);
        m_BufferVa = nullptr;
    }
    m_BufferBytes = 0;
    m_Allocated   = FALSE;
}

/* IMiniportWaveRTStreamNotification ------------------------------------- */

STDMETHODIMP
CMiniportWaveRTStream::AllocateBufferWithNotification(
    _In_  ULONG               NotificationCount,
    _In_  ULONG               RequestedSize,
    _Out_ PMDL*               OutMdl,
    _Out_ ULONG*              OutAllocatedSize,
    _Out_ ULONG*              OutOffsetFromFirstPage,
    _Out_ MEMORY_CACHING_TYPE* OutCacheType)
{
    PAGED_CODE();
    if (NotificationCount == 0) {
        NotificationCount = 1;
    }
    if (NotificationCount > 64) {
        NotificationCount = 64;
    }
    /* Round RequestedSize to a multiple of (frame * NotificationCount)
     * so each notification chunk is a whole number of frames. */
    ULONG chunk = STREAM_TO_SPEAKER_FRAME_BYTES * NotificationCount;
    if (chunk == 0) {
        chunk = STREAM_TO_SPEAKER_FRAME_BYTES;
    }
    RequestedSize -= (RequestedSize % chunk);
    if (RequestedSize == 0) {
        RequestedSize = chunk;
    }
    m_NotificationsPerBuffer = NotificationCount;
    /* Reuse AllocateAudioBuffer for the actual allocation — it picks up
     * m_NotificationsPerBuffer from the line above to compute
     * m_BytesPerNotification. */
    return AllocateAudioBuffer(RequestedSize, OutMdl, OutAllocatedSize,
                               OutOffsetFromFirstPage, OutCacheType);
}

STDMETHODIMP_(VOID)
CMiniportWaveRTStream::FreeBufferWithNotification(
    _In_opt_ PMDL Mdl,
    _In_     ULONG Size)
{
    PAGED_CODE();
    FreeAudioBuffer(Mdl, Size);
    m_NotificationsPerBuffer = 0;
    m_BytesPerNotification   = 0;
}

STDMETHODIMP
CMiniportWaveRTStream::RegisterNotificationEvent(_In_ PKEVENT NotificationEvent)
{
    PAGED_CODE();
    if (NotificationEvent == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    KIRQL old;
    KeAcquireSpinLock(&m_EventLock, &old);
    NTSTATUS status = STATUS_INSUFFICIENT_RESOURCES;
    for (ULONG i = 0; i < STREAM_TO_SPEAKER_MAX_NOTIFICATION_EVENTS; ++i) {
        if (m_NotificationEvents[i] == nullptr) {
            m_NotificationEvents[i] = NotificationEvent;
            if (i + 1 > m_NotificationEventCount) {
                m_NotificationEventCount = i + 1;
            }
            status = STATUS_SUCCESS;
            break;
        }
    }
    KeReleaseSpinLock(&m_EventLock, old);
    return status;
}

STDMETHODIMP
CMiniportWaveRTStream::UnregisterNotificationEvent(_In_ PKEVENT NotificationEvent)
{
    PAGED_CODE();
    if (NotificationEvent == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    KIRQL old;
    KeAcquireSpinLock(&m_EventLock, &old);
    NTSTATUS status = STATUS_NOT_FOUND;
    ULONG highWater = 0;
    for (ULONG i = 0; i < STREAM_TO_SPEAKER_MAX_NOTIFICATION_EVENTS; ++i) {
        if (m_NotificationEvents[i] == NotificationEvent) {
            m_NotificationEvents[i] = nullptr;
            status = STATUS_SUCCESS;
        }
        if (m_NotificationEvents[i] != nullptr) {
            highWater = i + 1;
        }
    }
    m_NotificationEventCount = highWater;
    KeReleaseSpinLock(&m_EventLock, old);
    return status;
}

STDMETHODIMP_(VOID)
CMiniportWaveRTStream::GetHWLatency(_Out_ PKSRTAUDIO_HWLATENCY OutLatency)
{
    PAGED_CODE();
    if (OutLatency == nullptr) {
        return;
    }
    OutLatency->FifoSize     = STREAM_TO_SPEAKER_FRAME_BYTES * 32;
    OutLatency->ChipsetDelay = 0;
    OutLatency->CodecDelay   = 0;
}

STDMETHODIMP
CMiniportWaveRTStream::GetPosition(_Out_ KSAUDIO_POSITION* OutPosition)
{
    if (OutPosition == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    if (m_BufferBytes == 0) {
        OutPosition->PlayOffset  = 0;
        OutPosition->WriteOffset = 0;
        return STATUS_SUCCESS;
    }
    /* PlayOffset and WriteOffset are byte offsets *within* the cyclic
     * buffer (mod m_BufferBytes), not cumulative byte counts.
     *
     * For a software-emulated render endpoint without a real DMA head
     * we follow sysvad-VirtualAudio: both offsets equal the wall-clock
     * play head. The engine then knows the entire region between the
     * advancing PlayOffset and the engine's own internal write head is
     * available, and refills accordingly. An earlier attempt put
     * WriteOffset one notification-interval AHEAD of PlayOffset; that
     * made the engine treat the queued depth as 2 ms and apparently
     * throttle, manifesting as ~1 packet per 80 s. */
    ULONGLONG playBytes = (ULONGLONG)m_StreamFramesProduced * STREAM_TO_SPEAKER_FRAME_BYTES;
    ULONG offset = (ULONG)(playBytes % (ULONGLONG)m_BufferBytes);
    OutPosition->PlayOffset  = offset;
    OutPosition->WriteOffset = offset;
    return STATUS_SUCCESS;
}

STDMETHODIMP
CMiniportWaveRTStream::GetPositionRegister(_Out_ KSRTAUDIO_HWREGISTER* OutRegister)
{
    PAGED_CODE();
    if (OutRegister == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlZeroMemory(OutRegister, sizeof(*OutRegister));
    /* No real register; engine polls GetPosition. */
    OutRegister->Register     = nullptr;
    OutRegister->Width        = 32;
    OutRegister->Numerator    = 1;
    OutRegister->Denominator  = 1;
    OutRegister->Accuracy     = STREAM_TO_SPEAKER_FRAME_BYTES;
    return STATUS_NOT_IMPLEMENTED;
}

STDMETHODIMP
CMiniportWaveRTStream::GetClockRegister(_Out_ KSRTAUDIO_HWREGISTER* OutRegister)
{
    PAGED_CODE();
    if (OutRegister == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlZeroMemory(OutRegister, sizeof(*OutRegister));
    return STATUS_NOT_IMPLEMENTED;
}

STDMETHODIMP
CMiniportWaveRTStream::SetFormat(_In_ PKSDATAFORMAT DataFormat)
{
    PAGED_CODE();
    UNREFERENCED_PARAMETER(DataFormat);
    /* We only advertise one format; PortCls validates it before we
     * see it. */
    return STATUS_SUCCESS;
}

STDMETHODIMP
CMiniportWaveRTStream::SetState(_In_ KSSTATE State)
{
    PAGED_CODE();

    KIRQL old;
    KeAcquireSpinLock(&m_StateLock, &old);
    KSSTATE prev = m_State;
    m_State = State;
    KeReleaseSpinLock(&m_StateLock, old);

    if (State == KSSTATE_RUN && prev != KSSTATE_RUN) {
        /* Reset frame counters and notify user-mode of stream start. */
        m_StreamFramesProduced = 0;
        m_StreamFramesConsumed = 0;
        m_LastNotificationConsumed = 0;
        m_LastTickQpc = KeQueryPerformanceCounter(&m_PerfFrequency);

        PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext = DeviceExtension();
        if (ext != nullptr && ext->IoctlCtx != nullptr) {
            IoctlOnStreamStart(ext->IoctlCtx);
        }
        StartTimer();
    } else if (State != KSSTATE_RUN && prev == KSSTATE_RUN) {
        StopTimer();
        PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext = DeviceExtension();
        if (ext != nullptr && ext->IoctlCtx != nullptr) {
            IoctlOnStreamStop(ext->IoctlCtx);
        }
    }

    return STATUS_SUCCESS;
}

/* ------------------------------------------------------------------ */
/* Timer / DPC                                                         */
/* ------------------------------------------------------------------ */

VOID
CMiniportWaveRTStream::StartTimer()
{
    if (m_TimerStarted) {
        return;
    }
    /* Raise the system timer resolution to 1 ms so our 2 ms periodic
     * DPC actually fires at the requested cadence. At Windows' default
     * 15.625 ms system tick, KeSetTimerEx(period=2) coalesces up to
     * ~16 ms — that drops the DPC rate from ~500/s to ~64/s and is
     * one of the suspects for the very-low packet rate. The Windows
     * audio engine usually bumps the resolution while a session is
     * active, but we shouldn't depend on it. */
    if (!m_TimerResolutionRaised) {
        (void)ExSetTimerResolution(10000u, TRUE);
        m_TimerResolutionRaised = TRUE;
    }
    m_TimerStarted = TRUE;
    KeSetTimerEx(&m_Timer,
                 m_TimerInterval,
                 (LONG)STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS,
                 &m_TimerDpc);
}

VOID
CMiniportWaveRTStream::StopTimer()
{
    if (!m_TimerStarted) {
        return;
    }
    KeCancelTimer(&m_Timer);
    KeFlushQueuedDpcs();
    m_TimerStarted = FALSE;
    if (m_TimerResolutionRaised) {
        (void)ExSetTimerResolution(0u, FALSE);
        m_TimerResolutionRaised = FALSE;
    }
}

VOID
CMiniportWaveRTStream::DoCopyToRing()
{
    /* Throttled diagnostic so we can confirm in DebugView that the DPC
     * is actually firing and how the produced/consumed counters are
     * advancing. Logs roughly once per second at the 2 ms cadence. */
    ++m_DpcLogCounter;
    if ((m_DpcLogCounter % 500u) == 1u) {
        DBG_INFO("DPC #%lu: produced=%llu consumed=%llu bufBytes=%lu notif/buf=%lu",
                 m_DpcLogCounter,
                 m_StreamFramesProduced,
                 m_StreamFramesConsumed,
                 m_BufferBytes,
                 m_NotificationsPerBuffer);
    }

    if (m_BufferVa == nullptr || m_BufferBytes == 0) {
        return;
    }

    /* Compute frames produced since last tick from elapsed QPC.
     * Advance m_LastTickQpc only by the *exact* QPC delta we converted
     * into whole frames; the fractional remainder stays in the
     * delta-to-next-tick so we don't lose drift over long runs. */
    LARGE_INTEGER nowQpc = KeQueryPerformanceCounter(nullptr);
    LONGLONG deltaTicks = nowQpc.QuadPart - m_LastTickQpc.QuadPart;
    if (deltaTicks <= 0 || m_PerfFrequency.QuadPart <= 0) {
        return;
    }
    /* frames = deltaTicks * sampleRate / freq */
    LONGLONG framesDelta = (deltaTicks * (LONGLONG)STREAM_TO_SPEAKER_SAMPLE_RATE)
                            / m_PerfFrequency.QuadPart;
    if (framesDelta <= 0) {
        return;
    }
    /* QPC ticks that correspond to exactly framesDelta frames; the
     * remainder gets carried over to the next tick. */
    LONGLONG consumedTicks =
        (framesDelta * m_PerfFrequency.QuadPart) / (LONGLONG)STREAM_TO_SPEAKER_SAMPLE_RATE;
    m_LastTickQpc.QuadPart += consumedTicks;
    m_StreamFramesProduced += (ULONGLONG)framesDelta;

    /* Number of frames we still owe the consumer. */
    ULONGLONG outstanding = m_StreamFramesProduced - m_StreamFramesConsumed;
    if (outstanding == 0) {
        return;
    }

    /* Cap at (bufferFrames - 1) — i.e., almost the full cyclic buffer.
     * A previous version capped at bufferFrames/2 which threw away
     * frames if the DPC was even slightly late, manifesting as a slow
     * loss until the consumer stalled. We can read up to the full
     * buffer minus one frame in a single pass because the engine's
     * write head is by construction ahead of where we read (PlayOffset
     * gates the engine's write window — see GetPosition). */
    ULONG bufferFrames = m_BufferBytes / STREAM_TO_SPEAKER_FRAME_BYTES;
    if (bufferFrames == 0) {
        return;
    }
    ULONGLONG maxThisTick = (bufferFrames > 1) ? (ULONGLONG)(bufferFrames - 1) : 1ull;
    if (outstanding > maxThisTick) {
        outstanding = maxThisTick;
    }
    if (outstanding == 0) {
        return;
    }

    /* Read position in the cyclic buffer = consumed frames mod bufferFrames. */
    ULONG readFrame = (ULONG)(m_StreamFramesConsumed % bufferFrames);
    ULONG bytesToRead = (ULONG)outstanding * STREAM_TO_SPEAKER_FRAME_BYTES;
    ULONG firstChunk = (bufferFrames - readFrame) * STREAM_TO_SPEAKER_FRAME_BYTES;
    if (firstChunk > bytesToRead) {
        firstChunk = bytesToRead;
    }

    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext = DeviceExtension();
    if (ext != nullptr && ext->IoctlCtx != nullptr) {
        IoctlAudioProduce(ext->IoctlCtx,
                          m_BufferVa + (readFrame * STREAM_TO_SPEAKER_FRAME_BYTES),
                          firstChunk);
        ULONG remaining = bytesToRead - firstChunk;
        if (remaining > 0) {
            IoctlAudioProduce(ext->IoctlCtx, m_BufferVa, remaining);
        }
    }

    m_StreamFramesConsumed += outstanding;

    /* Signal any registered notification events when we've crossed a
     * notification boundary. The audio engine uses these to schedule
     * its next buffer fill; without them it falls back to polling
     * GetPosition at 10-20 ms cadence, which starves a 4 ms buffer. */
    SignalNotificationEvents();
}

VOID
CMiniportWaveRTStream::SignalNotificationEvents()
{
    if (m_BytesPerNotification == 0) {
        return;
    }
    ULONGLONG consumedBytes = m_StreamFramesConsumed * STREAM_TO_SPEAKER_FRAME_BYTES;
    ULONGLONG lastBytes     = m_LastNotificationConsumed * STREAM_TO_SPEAKER_FRAME_BYTES;
    if (consumedBytes - lastBytes < m_BytesPerNotification) {
        return;
    }
    /* Round down to a notification boundary so we don't drift. */
    ULONGLONG boundary = (consumedBytes / m_BytesPerNotification)
                         * (ULONGLONG)m_BytesPerNotification;
    m_LastNotificationConsumed = boundary / STREAM_TO_SPEAKER_FRAME_BYTES;

    /* DPC-level: already at DISPATCH_LEVEL, no KIRQL save needed. */
    KeAcquireSpinLockAtDpcLevel(&m_EventLock);
    for (ULONG i = 0; i < m_NotificationEventCount; ++i) {
        PKEVENT ev = m_NotificationEvents[i];
        if (ev != nullptr) {
            /* KeSetEvent at DISPATCH_LEVEL: Increment IO_NO_INCREMENT,
             * Wait FALSE so we don't try to wait while at DISPATCH. */
            KeSetEvent(ev, IO_NO_INCREMENT, FALSE);
        }
    }
    KeReleaseSpinLockFromDpcLevel(&m_EventLock);
}

VOID
CMiniportWaveRTStream::OnConsumerDpc()
{
    /* Runs at DISPATCH_LEVEL. */
    KIRQL old;
    KeAcquireSpinLockAtDpcLevel(&m_StateLock);
    KSSTATE st = m_State;
    KeReleaseSpinLockFromDpcLevel(&m_StateLock);
    UNREFERENCED_PARAMETER(old);

    if (st != KSSTATE_RUN) {
        return;
    }
    DoCopyToRing();
}
