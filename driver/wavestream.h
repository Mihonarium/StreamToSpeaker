/*
 * wavestream.h - WaveRT stream object declaration.
 *
 * The stream owns:
 *  - The WaveRT cyclic buffer (the Windows audio engine writes here).
 *  - A "consumed position" in frames that tracks how far our consumer
 *    DPC has copied into the IOCTL ring buffer.
 *  - A periodic kernel timer (KTIMER + KDPC) that fires every
 *    STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS while in KSSTATE_RUN.
 *
 * Besides the classic position/notification model it implements
 * IMiniportWaveRTOutputStream (packet-based streaming: SetWritePacket,
 * GetPacketCount, presentation position). The audio engine only treats
 * a WaveRT render pin as event-driven capable when those are present,
 * and HLK's WaveRT conformance test requires it.
 */

#pragma once

#include "driver.h"
#include "ioctl.h"

class CMiniportWaveRT;

/* Maximum simultaneously-registered notification events. The audio
 * engine typically registers 1; sysvad supports a small handful. */
#define STREAM_TO_SPEAKER_MAX_NOTIFICATION_EVENTS  8

/* IMiniportWaveRTStreamNotification : IMiniportWaveRTStream (per
 * portcls.h), so inheriting just the Notification interface gives us
 * IMiniportWaveRTStream "for free" — and avoids the C4584 ambiguous-
 * base warning that fires when both are listed explicitly. */
class CMiniportWaveRTStream :
    public IMiniportWaveRTStreamNotification,
    public IMiniportWaveRTOutputStream,
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

    /* IMiniportWaveRTOutputStream — packet-based streaming. A "packet"
     * is one notification interval of the cyclic buffer. */
    STDMETHODIMP_(NTSTATUS) SetWritePacket(
        _In_ ULONG PacketNumber,
        _In_ DWORD Flags,
        _In_ ULONG EosPacketLength) override;
    STDMETHODIMP_(NTSTATUS) GetOutputStreamPresentationPosition(
        _Out_ KSAUDIO_PRESENTATION_POSITION* pPresentationPosition) override;
    STDMETHODIMP_(NTSTATUS) GetPacketCount(_Out_ ULONG* pPacketCount) override;

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

    /* Stream format: 1 or 2 channels of L16 @ 44.1 kHz. m_FrameBytes is
     * the engine-side frame size (2 or 4 bytes); the ring buffer the
     * service reads is always stereo (STREAM_TO_SPEAKER_FRAME_BYTES). */
    ULONG                  m_Channels;
    ULONG                  m_FrameBytes;

    /* WaveRT cyclic buffer. */
    PMDL                   m_BufferMdl;
    UCHAR*                 m_BufferVa;
    ULONG                  m_BufferBytes;     /* round to whole frame */

    /* Mono → stereo up-mix scratch space (m_BufferBytes * 2), only
     * allocated for mono streams. */
    UCHAR*                 m_UpmixVa;
    ULONG                  m_UpmixBytes;

    /* Frames "played" so far since the last KSSTATE_STOP (== Windows'
     * Play position in frames). Synthesised from the sample clock:
     * frames at the most recent RUN transition plus elapsed QPC time
     * since then. */
    ULONGLONG              m_StreamFramesProduced;
    ULONGLONG              m_FramesAtRunStart;
    LARGE_INTEGER          m_RunStartQpc;

    /* Frames our consumer has copied into the IOCTL ring already. */
    ULONGLONG              m_StreamFramesConsumed;

    /* Packet bookkeeping for IMiniportWaveRTOutputStream. */
    ULONG                  m_LastOsWritePacket;
    BOOLEAN                m_EosReceived;
    ULONG                  m_EosPacketNumber;
    ULONG                  m_EosPacketLength;

    /* Bookkeeping for the periodic DPC. */
    KTIMER                 m_Timer;
    KDPC                   m_TimerDpc;
    LARGE_INTEGER          m_TimerInterval;       /* relative, 100-ns units */
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

    VOID  ResetPosition();
    ULONG PacketBytes() const;
    ULONG PacketsCompleted() const;
    VOID  ProduceToRing(_In_reads_bytes_(Bytes) const UCHAR* Src, _In_ ULONG Bytes);
    VOID  StartTimer();
    VOID  StopTimer();
    VOID  DoCopyToRing();
    VOID  SignalNotificationEvents();
};
