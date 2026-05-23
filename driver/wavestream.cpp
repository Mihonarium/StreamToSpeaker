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
    if (IsEqualGUIDAligned(Interface, IID_IUnknown)) {
        *Object = PVOID(PUNKNOWN(PMINIPORTWAVERTSTREAM(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniportWaveRTStream)) {
        *Object = PVOID(PMINIPORTWAVERTSTREAM(this));
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
    KeInitializeTimer(&m_Timer);
    KeInitializeDpc(&m_TimerDpc, ConsumerDpcRoutine, this);
    KeInitializeSpinLock(&m_StateLock);

    /* 2 ms in 100-ns units = -20000 (negative means relative). */
    m_TimerInterval.QuadPart =
        -(LONGLONG)STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS * 10000LL;

    LARGE_INTEGER freq;
    m_StreamStartQpc = KeQueryPerformanceCounter(&freq);
    m_PerfFrequency  = freq;

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
     * buffer (mod m_BufferBytes), not cumulative byte counts. Returning
     * unbounded counts breaks the engine's modular arithmetic after
     * one buffer's worth of "consumed" bytes (~23 ms on a 4 KB buffer).
     *
     * WriteOffset must lead PlayOffset so the engine has a non-zero
     * safe window to write into. With WriteOffset == PlayOffset the
     * engine sees no write headroom and effectively stops feeding the
     * buffer — which is the lead suspect for our "rare 8-packet burst"
     * symptom. Pattern follows sysvad. */
    ULONGLONG playBytes = m_StreamFramesProduced * STREAM_TO_SPEAKER_FRAME_BYTES;
    ULONG notificationFrames = (STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS *
                                STREAM_TO_SPEAKER_SAMPLE_RATE) / 1000u;
    ULONG notificationBytes = notificationFrames * STREAM_TO_SPEAKER_FRAME_BYTES;
    OutPosition->PlayOffset  = playBytes % m_BufferBytes;
    OutPosition->WriteOffset = (playBytes + notificationBytes) % m_BufferBytes;
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

    /* Log every transition: we want to know if the pin ever reaches RUN
     * (the only state in which audio actually flows) and how often the
     * engine cycles it. KSSTATE: 0=STOP, 1=ACQUIRE, 2=PAUSE, 3=RUN. */
    if (prev != State) {
        DBG_INFO("SetState: %d -> %d", (int)prev, (int)State);
    }

    if (State == KSSTATE_RUN && prev != KSSTATE_RUN) {
        /* Reset frame counters and capture stream-start QPC. Producer
         * frames are computed *absolutely* from elapsed QPC ticks in
         * DoCopyToRing so per-tick truncation doesn't accumulate. */
        m_StreamFramesProduced = 0;
        m_StreamFramesConsumed = 0;
        m_StreamStartQpc = KeQueryPerformanceCounter(&m_PerfFrequency);

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
     * advancing. Logs roughly once per second at the 2 ms cadence.
     * The Ctx pointer is so we can compare against the consumer-side
     * "IRP queued / completed" logs in ioctl.cpp — if these pointers
     * disagree, audio is being produced into a different IoctlCtx than
     * the one user-mode IRPs are reaching. */
    ++m_DpcLogCounter;
    if ((m_DpcLogCounter % 500u) == 1u) {
        PSTREAM_TO_SPEAKER_DEVICE_EXTENSION extLog = DeviceExtension();
        void* ctxLog = (extLog != nullptr) ? (void*)extLog->IoctlCtx : nullptr;
        DBG_INFO("DPC #%lu: produced=%llu consumed=%llu bufBytes=%lu ctx=%p",
                 m_DpcLogCounter,
                 m_StreamFramesProduced,
                 m_StreamFramesConsumed,
                 m_BufferBytes,
                 ctxLog);
    }

    if (m_BufferVa == nullptr || m_BufferBytes == 0) {
        return;
    }

    /* Compute total frames produced since stream start absolutely from
     * elapsed QPC. The previous per-tick `framesDelta = deltaTicks *
     * SR / freq; m_LastTickQpc = nowQpc;` lost the truncation remainder
     * on every tick, which accumulated into ~0.5% sample-clock drift
     * (measured: 43,857 fps vs. expected 44,100). With Sonos playing
     * back from its own 44.1 kHz clock that drift slowly drains its
     * input buffer and causes the "pauses of growing length, then
     * disconnect" symptom. Computing total frames as one division
     * keeps the rounding error bounded to ±1 frame at any moment
     * instead of compounding. */
    LARGE_INTEGER nowQpc = KeQueryPerformanceCounter(nullptr);
    LONGLONG totalTicks = nowQpc.QuadPart - m_StreamStartQpc.QuadPart;
    if (totalTicks <= 0 || m_PerfFrequency.QuadPart <= 0) {
        return;
    }
    ULONGLONG totalFrames =
        ((ULONGLONG)totalTicks * (ULONGLONG)STREAM_TO_SPEAKER_SAMPLE_RATE)
        / (ULONGLONG)m_PerfFrequency.QuadPart;
    if (totalFrames <= m_StreamFramesProduced) {
        return;
    }
    m_StreamFramesProduced = totalFrames;

    /* Number of frames we still owe the consumer. */
    ULONGLONG outstanding = m_StreamFramesProduced - m_StreamFramesConsumed;
    if (outstanding == 0) {
        return;
    }

    /* Cap at half the cyclic buffer per tick — that's the safe window
     * to read without colliding with the engine's writer. */
    ULONG bufferFrames = m_BufferBytes / STREAM_TO_SPEAKER_FRAME_BYTES;
    ULONGLONG maxThisTick = bufferFrames / 2;
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
