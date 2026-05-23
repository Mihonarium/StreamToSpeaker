/*
 * wavestream.h - WaveRT stream object declaration.
 *
 * The stream owns:
 *  - The WaveRT cyclic buffer (the Windows audio engine writes here).
 *  - A "consumed position" in frames that tracks how far our consumer
 *    DPC has copied into the IOCTL ring buffer.
 *  - A periodic kernel timer (KTIMER + KDPC) that fires every
 *    STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS while in KSSTATE_RUN.
 */

#pragma once

#include "driver.h"
#include "ioctl.h"

class CMiniportWaveRT;

/* Maximum simultaneously-registered notification events. The audio
 * engine typically registers 1; sysvad supports a small handful. */
#define STREAM_TO_SPEAKER_MAX_NOTIFICATION_EVENTS  8

class CMiniportWaveRTStream :
    public IMiniportWaveRTStream,
    public IMiniportWaveRTStreamNotification,
    public CUnknown
{
public:
    DECLARE_STD_UNKNOWN();
    DEFINE_STD_CONSTRUCTOR(CMiniportWaveRTStream);
    ~CMiniportWaveRTStream();

    NTSTATUS Init(
        _In_ CMiniportWaveRT*  Miniport,
        _In_ PPORTWAVERTSTREAM PortStream,
        _In_ ULONG             Pin,
        _In_ PKSDATAFORMAT     DataFormat);

    /* IMiniportWaveRTStream */
    STDMETHODIMP AllocateAudioBuffer(
        _In_  ULONG          RequestedSize,
        _Out_ PMDL*          OutMdl,
        _Out_ ULONG*         OutAllocatedSize,
        _Out_ ULONG*         OutOffset,
        _Out_ MEMORY_CACHING_TYPE* OutCacheType) override;

    STDMETHODIMP_(VOID) FreeAudioBuffer(
        _In_opt_ PMDL Mdl,
        _In_     ULONG Size) override;

    /* IMiniportWaveRTStream::GetHWLatency is VOID-returning in current
     * portcls.h (10.0.26100). The output struct carries any status. */
    STDMETHODIMP_(VOID) GetHWLatency(_Out_ PKSRTAUDIO_HWLATENCY OutLatency) override;

    STDMETHODIMP GetPosition(_Out_ KSAUDIO_POSITION* OutPosition) override;

    STDMETHODIMP GetPositionRegister(_Out_ KSRTAUDIO_HWREGISTER* OutRegister) override;

    STDMETHODIMP GetClockRegister(_Out_ KSRTAUDIO_HWREGISTER* OutRegister) override;

    STDMETHODIMP SetFormat(_In_ PKSDATAFORMAT DataFormat) override;
    STDMETHODIMP SetState(_In_ KSSTATE State) override;

    /* IMiniportWaveRTStreamNotification — event-based wakeup for the
     * audio engine. Without this, the engine has to poll GetPosition,
     * typically at 10-20 ms cadence, which starves a 4 ms cyclic
     * buffer. Signalling at every DPC tick (2 ms) keeps the engine
     * writing at near-real-time pace. */
    STDMETHODIMP AllocateBufferWithNotification(
        _In_  ULONG               NotificationCount,
        _In_  ULONG               RequestedSize,
        _Out_ PMDL*               OutMdl,
        _Out_ ULONG*              OutAllocatedSize,
        _Out_ ULONG*              OutOffsetFromFirstPage,
        _Out_ MEMORY_CACHING_TYPE* OutCacheType) override;

    STDMETHODIMP_(VOID) FreeBufferWithNotification(
        _In_opt_ PMDL Mdl,
        _In_     ULONG Size) override;

    STDMETHODIMP RegisterNotificationEvent(_In_ PKEVENT NotificationEvent) override;
    STDMETHODIMP UnregisterNotificationEvent(_In_ PKEVENT NotificationEvent) override;

    /* The DPC handler that copies fresh frames out of the WaveRT
     * cyclic buffer and into the IOCTL ring buffer. Public so the
     * static C-style KDEFERRED_ROUTINE thunk can call into it. */
    VOID OnConsumerDpc();

private:
    CMiniportWaveRT*       m_Miniport;
    PPORTWAVERTSTREAM      m_PortStream;
    ULONG                  m_PinId;
    BOOLEAN                m_Allocated;
    KSSTATE                m_State;

    /* WaveRT cyclic buffer. */
    PMDL                   m_BufferMdl;
    UCHAR*                 m_BufferVa;
    ULONG                  m_BufferBytes;     /* round to whole frame */

    /* Frames written by the engine so far (== Windows' Play position
     * in samples). We track this with a sample-counter that we
     * increment in the DPC by the elapsed time delta. */
    ULONGLONG              m_StreamFramesProduced;

    /* Frames our consumer has copied into the IOCTL ring already. */
    ULONGLONG              m_StreamFramesConsumed;

    /* Bookkeeping for the periodic DPC. */
    KTIMER                 m_Timer;
    KDPC                   m_TimerDpc;
    LARGE_INTEGER          m_TimerInterval;       /* relative, 100-ns units */
    LARGE_INTEGER          m_LastTickQpc;
    LARGE_INTEGER          m_PerfFrequency;

    KSPIN_LOCK             m_StateLock;
    BOOLEAN                m_TimerStarted;
    BOOLEAN                m_TimerResolutionRaised;
    ULONG                  m_DpcLogCounter;

    /* Notification events registered by the audio engine via
     * RegisterNotificationEvent. Signalled at each notification
     * boundary by the DPC. Protected by m_EventLock. */
    KSPIN_LOCK             m_EventLock;
    PKEVENT                m_NotificationEvents[STREAM_TO_SPEAKER_MAX_NOTIFICATION_EVENTS];
    ULONG                  m_NotificationEventCount;
    ULONG                  m_NotificationsPerBuffer;  /* from AllocateBufferWithNotification */
    ULONG                  m_BytesPerNotification;    /* m_BufferBytes / NotificationsPerBuffer */
    ULONGLONG              m_LastNotificationConsumed; /* frame count at last signal */

    /* Convenience accessor for the device extension. */
    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION DeviceExtension();

    VOID StartTimer();
    VOID StopTimer();
    VOID DoCopyToRing();
    VOID SignalNotificationEvents();
};
