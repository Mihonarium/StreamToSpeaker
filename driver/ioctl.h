/* Forward decl — defined in driver.cpp. Returns the currently-published
 * IoctlCtx pointer, or nullptr if no audio FDO is up yet. Safe to call
 * at any IRQL <= DISPATCH_LEVEL. Used by the IOCTL dispatch and cancel
 * routines on the control device, which has no DeviceExtension. */
struct StreamToSpeakerIoctlCtx;
extern "C" StreamToSpeakerIoctlCtx* StreamToSpeakerCurrentIoctlCtx();

/*
 * ioctl.h - IRP_MJ_DEVICE_CONTROL dispatch and pended-IRP queue.
 *
 * The driver exposes four IOCTLs (see include/stream_to_speaker_ioctl.h):
 *
 *   GET_AUDIO_PACKET   pended, METHOD_OUT_DIRECT, drained by WaveRT DPC
 *   GET_CONTROL_EVENT  pended, METHOD_BUFFERED,  posted by KS setters
 *                                                 and stream lifecycle
 *   PUSH_VOLUME        synchronous
 *   GET_VERSION        synchronous
 *
 * Pended IRPs are tracked in a doubly-linked LIST_ENTRY guarded by a
 * KSPIN_LOCK. Each pended IRP has a cancel routine installed so that
 * a client closing the handle (or calling CancelIoEx) frees us cleanly.
 *
 * The ring buffer holds raw PCM only; we synthesize the
 * STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER at the moment we complete an IRP.
 *
 * Control events are stored in a small circular array of
 * STREAM_TO_SPEAKER_CONTROL_EVENT structs; when an IRP is waiting we deliver
 * the next event immediately, otherwise we queue up to a small number
 * (oldest dropped on overflow).
 */

#pragma once

#include "driver.h"
#include "ringbuffer.h"

#define STREAM_TO_SPEAKER_EVENT_QUEUE_DEPTH    16

struct StreamToSpeakerIoctlCtx {
    /* Set once at AddDevice; cleared on RemoveDevice. */
    PDEVICE_OBJECT          DeviceObject;

    /* Pending audio IRPs (IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET). */
    LIST_ENTRY              AudioIrpList;
    KSPIN_LOCK              AudioIrpLock;

    /* Pending control-event IRPs (IOCTL_STREAM_TO_SPEAKER_GET_CONTROL_EVENT). */
    LIST_ENTRY              EventIrpList;
    KSPIN_LOCK              EventIrpLock;

    /* Audio ring buffer (PCM bytes). */
    StreamToSpeakerRingBuffer     AudioRing;

    /* Queued control events when no IRP is waiting. */
    STREAM_TO_SPEAKER_CONTROL_EVENT EventQueue[STREAM_TO_SPEAKER_EVENT_QUEUE_DEPTH];
    ULONG                   EventHead;
    ULONG                   EventTail;
    KSPIN_LOCK              EventQueueLock;

    /* Monotonic stream position (frames). Reset on stream start.
     * Touched only from the WaveRT DPC path. */
    UINT64                  StreamPositionFrames;

    /* Latch the "next packet has STREAM_RESTART" flag. Set on stream
     * start, cleared after first packet emitted. */
    volatile LONG           PendingStreamRestart;
};

/* Lifecycle. */
NTSTATUS IoctlCtxInit(_Inout_ StreamToSpeakerIoctlCtx* Ctx,
                      _In_ PDEVICE_OBJECT DeviceObject);
VOID     IoctlCtxDestroy(_Inout_ StreamToSpeakerIoctlCtx* Ctx);

/* Main dispatch (called from StreamToSpeakerDispatchDeviceControl). Returns
 * the status to put in the IRP (or STATUS_PENDING if the IRP was
 * queued). Caller is responsible for IoCompleteRequest unless we
 * returned STATUS_PENDING. */
NTSTATUS IoctlDispatch(_In_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp);

/* WaveRT DPC entry points. */

/* Producer: push fresh PCM bytes into the ring buffer. Bytes is the
 * number of new frames * STREAM_TO_SPEAKER_FRAME_BYTES. Called at
 * DISPATCH_LEVEL from the WaveRT consumer DPC. */
VOID IoctlAudioProduce(
    _Inout_ StreamToSpeakerIoctlCtx* Ctx,
    _In_reads_bytes_(Bytes) const VOID* Pcm,
    _In_ ULONG Bytes);

/* Try to drain pending audio IRPs. Called after every produce, and
 * also on demand. */
VOID IoctlTryCompleteAudio(_Inout_ StreamToSpeakerIoctlCtx* Ctx);

/* Post a control event. If an IRP is waiting, complete it
 * immediately; otherwise enqueue (dropping oldest on overflow). */
VOID IoctlPostEvent(_Inout_ StreamToSpeakerIoctlCtx* Ctx,
                    _In_ const STREAM_TO_SPEAKER_CONTROL_EVENT* Event);

/* Stream lifecycle hooks (called from CMiniportWaveRTStream::SetState
 * etc). */
VOID IoctlOnStreamStart(_Inout_ StreamToSpeakerIoctlCtx* Ctx);
VOID IoctlOnStreamStop (_Inout_ StreamToSpeakerIoctlCtx* Ctx);

/* Cancel all pended IRPs. Called on device remove. */
VOID IoctlCancelAll(_Inout_ StreamToSpeakerIoctlCtx* Ctx);
