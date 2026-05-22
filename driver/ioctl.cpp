/*
 * ioctl.cpp - IRP_MJ_DEVICE_CONTROL implementation.
 *
 * Implements the inverted-call pattern used by the user-mode bridge
 * service:
 *
 *   - user-mode posts GET_AUDIO_PACKET; if no audio is ready, the IRP
 *     is queued. When the WaveRT DPC delivers fresh PCM into the ring
 *     buffer, we drain queued IRPs and complete them.
 *   - user-mode posts GET_CONTROL_EVENT; if events are queued, we
 *     complete immediately. Otherwise the IRP is queued and completed
 *     when a topology setter or stream-lifecycle hook fires.
 *
 * Cancellation safety follows the standard pattern: every pended IRP
 * has IoSetCancelRoutine(StreamToSpeakerIoctlCancelRoutine) installed; the
 * routine acquires the list lock, removes the IRP if still present,
 * and completes with STATUS_CANCELLED. Before completing an IRP from
 * any other path we call IoSetCancelRoutine(Irp, NULL) and check the
 * old routine — if it was already nulled by the I/O manager the IRP
 * is being cancelled and we must leave it alone.
 */

#include "ioctl.h"

/* Forward decls for static helpers. */
static DRIVER_CANCEL StreamToSpeakerIoctlCancelAudio;
static DRIVER_CANCEL StreamToSpeakerIoctlCancelEvent;

static NTSTATUS Ioctl_HandleGetVersion (_In_ PIRP Irp);
static NTSTATUS Ioctl_HandleGetAudio   (_Inout_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp);
static NTSTATUS Ioctl_HandleGetEvent   (_Inout_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp);
static NTSTATUS Ioctl_HandlePushVolume (_Inout_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp);

/* ------------------------------------------------------------------ */
/* Lifecycle                                                           */
/* ------------------------------------------------------------------ */

NTSTATUS
IoctlCtxInit(_Inout_ StreamToSpeakerIoctlCtx* Ctx,
             _In_ PDEVICE_OBJECT DeviceObject)
{
    if (Ctx == nullptr || DeviceObject == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlZeroMemory(Ctx, sizeof(*Ctx));

    Ctx->DeviceObject = DeviceObject;

    InitializeListHead(&Ctx->AudioIrpList);
    InitializeListHead(&Ctx->EventIrpList);
    KeInitializeSpinLock(&Ctx->AudioIrpLock);
    KeInitializeSpinLock(&Ctx->EventIrpLock);
    KeInitializeSpinLock(&Ctx->EventQueueLock);

    NTSTATUS status = RingBufferInit(&Ctx->AudioRing, STREAM_TO_SPEAKER_RING_BYTES);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    Ctx->EventHead = 0;
    Ctx->EventTail = 0;
    Ctx->StreamPositionFrames = 0;
    Ctx->PendingStreamRestart = 0;

    return STATUS_SUCCESS;
}

VOID
IoctlCtxDestroy(_Inout_ StreamToSpeakerIoctlCtx* Ctx)
{
    if (Ctx == nullptr) {
        return;
    }
    IoctlCancelAll(Ctx);
    RingBufferDestroy(&Ctx->AudioRing);
    Ctx->DeviceObject = nullptr;
}

/* ------------------------------------------------------------------ */
/* Dispatch                                                            */
/* ------------------------------------------------------------------ */

NTSTATUS
IoctlDispatch(_In_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp)
{
    PAGED_CODE();

    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    NTSTATUS status;

    switch (sp->Parameters.DeviceIoControl.IoControlCode) {
    case IOCTL_STREAM_TO_SPEAKER_GET_VERSION:
        status = Ioctl_HandleGetVersion(Irp);
        break;

    case IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET:
        status = Ioctl_HandleGetAudio(Ctx, Irp);
        break;

    case IOCTL_STREAM_TO_SPEAKER_GET_CONTROL_EVENT:
        status = Ioctl_HandleGetEvent(Ctx, Irp);
        break;

    case IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME:
        status = Ioctl_HandlePushVolume(Ctx, Irp);
        break;

    default:
        status = STATUS_INVALID_DEVICE_REQUEST;
        Irp->IoStatus.Information = 0;
        break;
    }

    return status;
}

/* ------------------------------------------------------------------ */
/* Synchronous handlers                                                */
/* ------------------------------------------------------------------ */

static NTSTATUS
Ioctl_HandleGetVersion(_In_ PIRP Irp)
{
    PAGED_CODE();
    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);

    if (sp->Parameters.DeviceIoControl.OutputBufferLength
            < sizeof(STREAM_TO_SPEAKER_VERSION_INFO)) {
        Irp->IoStatus.Information = 0;
        return STATUS_BUFFER_TOO_SMALL;
    }

    STREAM_TO_SPEAKER_VERSION_INFO* out =
        static_cast<STREAM_TO_SPEAKER_VERSION_INFO*>(Irp->AssociatedIrp.SystemBuffer);
    if (out == nullptr) {
        Irp->IoStatus.Information = 0;
        return STATUS_INVALID_PARAMETER;
    }

    out->ProtocolVersion = STREAM_TO_SPEAKER_PROTOCOL_VERSION;
    out->DriverBuild     = STREAM_TO_SPEAKER_DRIVER_BUILD;

    Irp->IoStatus.Information = sizeof(*out);
    return STATUS_SUCCESS;
}

static NTSTATUS
Ioctl_HandlePushVolume(_Inout_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp)
{
    PAGED_CODE();
    UNREFERENCED_PARAMETER(Ctx);

    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    if (sp->Parameters.DeviceIoControl.InputBufferLength
            < sizeof(STREAM_TO_SPEAKER_PUSH_VOLUME_INPUT)) {
        Irp->IoStatus.Information = 0;
        return STATUS_BUFFER_TOO_SMALL;
    }

    STREAM_TO_SPEAKER_PUSH_VOLUME_INPUT* in =
        static_cast<STREAM_TO_SPEAKER_PUSH_VOLUME_INPUT*>(Irp->AssociatedIrp.SystemBuffer);
    if (in == nullptr) {
        Irp->IoStatus.Information = 0;
        return STATUS_INVALID_PARAMETER;
    }

    INT32 mb = in->LevelMillibels;
    if (mb < STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS) {
        mb = STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS;
    } else if (mb > STREAM_TO_SPEAKER_VOLUME_MAX_MILLIBELS) {
        mb = STREAM_TO_SPEAKER_VOLUME_MAX_MILLIBELS;
    }

    /* Hand off to topology so it can fire a KS property change event
     * back to the Windows audio engine. The topology pointer is held
     * in the device extension. */
    PDEVICE_OBJECT devObj = Ctx->DeviceObject;
    if (devObj != nullptr) {
        PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext =
            StreamToSpeakerGetExt(devObj);
        if (ext != nullptr && ext->Topology != nullptr) {
            extern VOID TopologyOnExternalVolumeChange(
                CMiniportTopology*, INT32, BOOLEAN);
            TopologyOnExternalVolumeChange(ext->Topology, mb, in->Muted != 0);
        }
    }

    Irp->IoStatus.Information = 0;
    return STATUS_SUCCESS;
}

/* ------------------------------------------------------------------ */
/* Audio IRP queue                                                     */
/* ------------------------------------------------------------------ */

/* Validate a queued audio IRP's output buffer and look up the MDL
 * system-address mapping. */
static NTSTATUS
GetAudioIrpOutputBuffer(_In_ PIRP Irp,
                        _Out_ PVOID* OutBuffer,
                        _Out_ ULONG* OutLength)
{
    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    *OutBuffer = nullptr;
    *OutLength = 0;

    ULONG cb = sp->Parameters.DeviceIoControl.OutputBufferLength;
    if (cb < sizeof(STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER) + STREAM_TO_SPEAKER_FRAME_BYTES) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    /* METHOD_OUT_DIRECT: Irp->MdlAddress describes the user buffer. */
    PMDL mdl = Irp->MdlAddress;
    if (mdl == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    PVOID sysAddr = MmGetSystemAddressForMdlSafe(mdl, NormalPagePriority | MdlMappingNoExecute);
    if (sysAddr == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    *OutBuffer = sysAddr;
    *OutLength = cb;
    return STATUS_SUCCESS;
}

static NTSTATUS
Ioctl_HandleGetAudio(_Inout_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp)
{
    PAGED_CODE();

    PVOID  outBuf = nullptr;
    ULONG  outLen = 0;
    NTSTATUS status = GetAudioIrpOutputBuffer(Irp, &outBuf, &outLen);
    if (!NT_SUCCESS(status)) {
        Irp->IoStatus.Information = 0;
        return status;
    }

    /* Mark pending, hook cancel routine, queue. */
    IoMarkIrpPending(Irp);

    KIRQL old;
    KeAcquireSpinLock(&Ctx->AudioIrpLock, &old);
    IoSetCancelRoutine(Irp, StreamToSpeakerIoctlCancelAudio);
    if (Irp->Cancel) {
        /* Already cancelled before we wired the cancel routine. Undo
         * and complete here. */
        if (IoSetCancelRoutine(Irp, nullptr) != nullptr) {
            KeReleaseSpinLock(&Ctx->AudioIrpLock, old);
            Irp->IoStatus.Status = STATUS_CANCELLED;
            Irp->IoStatus.Information = 0;
            IoCompleteRequest(Irp, IO_NO_INCREMENT);
            return STATUS_PENDING; /* IRP already completed by us. */
        }
        /* Cancel routine already ran; it will complete the IRP. */
        KeReleaseSpinLock(&Ctx->AudioIrpLock, old);
        return STATUS_PENDING;
    }
    InsertTailList(&Ctx->AudioIrpList, &Irp->Tail.Overlay.ListEntry);
    KeReleaseSpinLock(&Ctx->AudioIrpLock, old);

    /* Try to drain right away in case there's already data waiting. */
    IoctlTryCompleteAudio(Ctx);
    return STATUS_PENDING;
}

/* Pull one IRP from the head of the list with the lock held by the
 * caller-equivalent of "remove with cancel race protection". Returns
 * nullptr if no IRP could be claimed. */
static PIRP
DequeueAudioIrp(_Inout_ StreamToSpeakerIoctlCtx* Ctx, _In_ KIRQL HoldingAtIrql)
{
    UNREFERENCED_PARAMETER(HoldingAtIrql);
    while (!IsListEmpty(&Ctx->AudioIrpList)) {
        PLIST_ENTRY entry = RemoveHeadList(&Ctx->AudioIrpList);
        PIRP irp = CONTAINING_RECORD(entry, IRP, Tail.Overlay.ListEntry);
        if (IoSetCancelRoutine(irp, nullptr) != nullptr) {
            return irp;
        }
        /* Cancel routine fired between our peek and the clear. Put
         * the entry back in a defensive zero state and let the cancel
         * routine complete it. */
        InitializeListHead(&irp->Tail.Overlay.ListEntry);
    }
    return nullptr;
}

VOID
IoctlAudioProduce(
    _Inout_ StreamToSpeakerIoctlCtx* Ctx,
    _In_reads_bytes_(Bytes) const VOID* Pcm,
    _In_ ULONG Bytes)
{
    if (Ctx == nullptr || Pcm == nullptr || Bytes == 0) {
        return;
    }
    (void) RingBufferWrite(&Ctx->AudioRing, Pcm, Bytes);
    /* Stream position is in frames. */
    Ctx->StreamPositionFrames += (Bytes / STREAM_TO_SPEAKER_FRAME_BYTES);
    IoctlTryCompleteAudio(Ctx);
}

VOID
IoctlTryCompleteAudio(_Inout_ StreamToSpeakerIoctlCtx* Ctx)
{
    if (Ctx == nullptr) {
        return;
    }
    /* This may be called at DISPATCH_LEVEL from the WaveRT DPC. */
    for (;;) {
        ULONG avail = RingBufferAvailable(&Ctx->AudioRing);
        if (avail < STREAM_TO_SPEAKER_FRAME_BYTES) {
            return;
        }

        KIRQL old;
        KeAcquireSpinLock(&Ctx->AudioIrpLock, &old);
        PIRP irp = DequeueAudioIrp(Ctx, old);
        KeReleaseSpinLock(&Ctx->AudioIrpLock, old);
        if (irp == nullptr) {
            return;
        }

        PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(irp);
        ULONG cb = sp->Parameters.DeviceIoControl.OutputBufferLength;

        PMDL mdl = irp->MdlAddress;
        PVOID sysAddr = (mdl != nullptr)
            ? MmGetSystemAddressForMdlSafe(mdl, NormalPagePriority | MdlMappingNoExecute)
            : nullptr;
        if (sysAddr == nullptr) {
            irp->IoStatus.Status = STATUS_INSUFFICIENT_RESOURCES;
            irp->IoStatus.Information = 0;
            IoCompleteRequest(irp, IO_NO_INCREMENT);
            continue;
        }

        STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER* hdr =
            static_cast<STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER*>(sysAddr);
        UCHAR* pcmOut = static_cast<UCHAR*>(sysAddr) + sizeof(*hdr);
        ULONG  pcmCap = cb - sizeof(*hdr);
        if (pcmCap > STREAM_TO_SPEAKER_MAX_PACKET_BYTES) {
            pcmCap = STREAM_TO_SPEAKER_MAX_PACKET_BYTES;
        }
        /* Round to a whole frame so the consumer never has to deal
         * with partial samples. */
        pcmCap -= (pcmCap % STREAM_TO_SPEAKER_FRAME_BYTES);

        ULONG copied = RingBufferRead(&Ctx->AudioRing, pcmOut, pcmCap);
        if (copied == 0) {
            /* Race: someone else drained the ring. Requeue at head. */
            IoMarkIrpPending(irp);
            KeAcquireSpinLock(&Ctx->AudioIrpLock, &old);
            IoSetCancelRoutine(irp, StreamToSpeakerIoctlCancelAudio);
            if (irp->Cancel && IoSetCancelRoutine(irp, nullptr) != nullptr) {
                KeReleaseSpinLock(&Ctx->AudioIrpLock, old);
                irp->IoStatus.Status = STATUS_CANCELLED;
                irp->IoStatus.Information = 0;
                IoCompleteRequest(irp, IO_NO_INCREMENT);
                continue;
            }
            InsertHeadList(&Ctx->AudioIrpList, &irp->Tail.Overlay.ListEntry);
            KeReleaseSpinLock(&Ctx->AudioIrpLock, old);
            return;
        }

        ULONG flags = 0;
        if (InterlockedExchange(&Ctx->PendingStreamRestart, 0) != 0) {
            flags |= STREAM_TO_SPEAKER_PACKET_FLAG_STREAM_RESTART;
        }

        LARGE_INTEGER qpc = KeQueryPerformanceCounter(nullptr);

        hdr->SampleRate       = STREAM_TO_SPEAKER_SAMPLE_RATE;
        hdr->BitsPerSample    = (UINT16)STREAM_TO_SPEAKER_BITS_PER_SAMPLE;
        hdr->Channels         = (UINT16)STREAM_TO_SPEAKER_CHANNELS;
        hdr->SampleFrameCount = copied / STREAM_TO_SPEAKER_FRAME_BYTES;
        hdr->DataBytes        = copied;
        hdr->Flags            = flags;
        hdr->TimestampQpc     = (UINT64)qpc.QuadPart;
        hdr->StreamPosition   = Ctx->StreamPositionFrames;

        irp->IoStatus.Status      = STATUS_SUCCESS;
        irp->IoStatus.Information = sizeof(*hdr) + copied;
        IoCompleteRequest(irp, IO_SOUND_INCREMENT);
    }
}

/* ------------------------------------------------------------------ */
/* Control-event IRP queue                                             */
/* ------------------------------------------------------------------ */

static NTSTATUS
Ioctl_HandleGetEvent(_Inout_ StreamToSpeakerIoctlCtx* Ctx, _In_ PIRP Irp)
{
    PAGED_CODE();
    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);

    if (sp->Parameters.DeviceIoControl.OutputBufferLength
            < sizeof(STREAM_TO_SPEAKER_CONTROL_EVENT)) {
        Irp->IoStatus.Information = 0;
        return STATUS_BUFFER_TOO_SMALL;
    }

    /* If an event is queued, complete immediately. */
    KIRQL old;
    KeAcquireSpinLock(&Ctx->EventQueueLock, &old);
    if (Ctx->EventHead != Ctx->EventTail) {
        STREAM_TO_SPEAKER_CONTROL_EVENT ev = Ctx->EventQueue[Ctx->EventHead];
        Ctx->EventHead = (Ctx->EventHead + 1) % STREAM_TO_SPEAKER_EVENT_QUEUE_DEPTH;
        KeReleaseSpinLock(&Ctx->EventQueueLock, old);

        STREAM_TO_SPEAKER_CONTROL_EVENT* out =
            static_cast<STREAM_TO_SPEAKER_CONTROL_EVENT*>(Irp->AssociatedIrp.SystemBuffer);
        if (out == nullptr) {
            Irp->IoStatus.Information = 0;
            return STATUS_INVALID_PARAMETER;
        }
        *out = ev;
        Irp->IoStatus.Information = sizeof(ev);
        return STATUS_SUCCESS;
    }
    KeReleaseSpinLock(&Ctx->EventQueueLock, old);

    /* Otherwise queue the IRP. */
    IoMarkIrpPending(Irp);
    KeAcquireSpinLock(&Ctx->EventIrpLock, &old);
    IoSetCancelRoutine(Irp, StreamToSpeakerIoctlCancelEvent);
    if (Irp->Cancel) {
        if (IoSetCancelRoutine(Irp, nullptr) != nullptr) {
            KeReleaseSpinLock(&Ctx->EventIrpLock, old);
            Irp->IoStatus.Status = STATUS_CANCELLED;
            Irp->IoStatus.Information = 0;
            IoCompleteRequest(Irp, IO_NO_INCREMENT);
            return STATUS_PENDING;
        }
        KeReleaseSpinLock(&Ctx->EventIrpLock, old);
        return STATUS_PENDING;
    }
    InsertTailList(&Ctx->EventIrpList, &Irp->Tail.Overlay.ListEntry);
    KeReleaseSpinLock(&Ctx->EventIrpLock, old);
    return STATUS_PENDING;
}

static PIRP
DequeueEventIrp(_Inout_ StreamToSpeakerIoctlCtx* Ctx)
{
    while (!IsListEmpty(&Ctx->EventIrpList)) {
        PLIST_ENTRY entry = RemoveHeadList(&Ctx->EventIrpList);
        PIRP irp = CONTAINING_RECORD(entry, IRP, Tail.Overlay.ListEntry);
        if (IoSetCancelRoutine(irp, nullptr) != nullptr) {
            return irp;
        }
        InitializeListHead(&irp->Tail.Overlay.ListEntry);
    }
    return nullptr;
}

VOID
IoctlPostEvent(_Inout_ StreamToSpeakerIoctlCtx* Ctx,
               _In_ const STREAM_TO_SPEAKER_CONTROL_EVENT* Event)
{
    if (Ctx == nullptr || Event == nullptr) {
        return;
    }

    KIRQL old;
    /* First try to hand it to a waiting IRP. */
    KeAcquireSpinLock(&Ctx->EventIrpLock, &old);
    PIRP irp = DequeueEventIrp(Ctx);
    KeReleaseSpinLock(&Ctx->EventIrpLock, old);
    if (irp != nullptr) {
        STREAM_TO_SPEAKER_CONTROL_EVENT* out =
            static_cast<STREAM_TO_SPEAKER_CONTROL_EVENT*>(irp->AssociatedIrp.SystemBuffer);
        PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(irp);
        if (out != nullptr &&
            sp->Parameters.DeviceIoControl.OutputBufferLength >= sizeof(*Event)) {
            *out = *Event;
            irp->IoStatus.Status = STATUS_SUCCESS;
            irp->IoStatus.Information = sizeof(*Event);
        } else {
            irp->IoStatus.Status = STATUS_BUFFER_TOO_SMALL;
            irp->IoStatus.Information = 0;
        }
        IoCompleteRequest(irp, IO_NO_INCREMENT);
        return;
    }

    /* No IRP — queue it (dropping oldest if full). */
    KeAcquireSpinLock(&Ctx->EventQueueLock, &old);
    ULONG nextTail = (Ctx->EventTail + 1) % STREAM_TO_SPEAKER_EVENT_QUEUE_DEPTH;
    if (nextTail == Ctx->EventHead) {
        /* Full — drop oldest. */
        Ctx->EventHead = (Ctx->EventHead + 1) % STREAM_TO_SPEAKER_EVENT_QUEUE_DEPTH;
    }
    Ctx->EventQueue[Ctx->EventTail] = *Event;
    Ctx->EventTail = nextTail;
    KeReleaseSpinLock(&Ctx->EventQueueLock, old);
}

/* ------------------------------------------------------------------ */
/* Stream lifecycle                                                    */
/* ------------------------------------------------------------------ */

VOID
IoctlOnStreamStart(_Inout_ StreamToSpeakerIoctlCtx* Ctx)
{
    if (Ctx == nullptr) {
        return;
    }
    RingBufferReset(&Ctx->AudioRing);
    Ctx->StreamPositionFrames = 0;
    InterlockedExchange(&Ctx->PendingStreamRestart, 1);

    STREAM_TO_SPEAKER_CONTROL_EVENT ev = { };
    ev.EventType = StreamToSpeakerEventStreamStart;
    IoctlPostEvent(Ctx, &ev);
}

VOID
IoctlOnStreamStop(_Inout_ StreamToSpeakerIoctlCtx* Ctx)
{
    if (Ctx == nullptr) {
        return;
    }

    /* Drain pended audio IRPs with zero-length completions so the
     * service knows to switch to silence-injection mode. */
    for (;;) {
        KIRQL old;
        KeAcquireSpinLock(&Ctx->AudioIrpLock, &old);
        PIRP irp = DequeueAudioIrp(Ctx, old);
        KeReleaseSpinLock(&Ctx->AudioIrpLock, old);
        if (irp == nullptr) {
            break;
        }
        irp->IoStatus.Status = STATUS_SUCCESS;
        irp->IoStatus.Information = 0;
        IoCompleteRequest(irp, IO_NO_INCREMENT);
    }

    STREAM_TO_SPEAKER_CONTROL_EVENT ev = { };
    ev.EventType = StreamToSpeakerEventStreamStop;
    IoctlPostEvent(Ctx, &ev);
}

VOID
IoctlCancelAll(_Inout_ StreamToSpeakerIoctlCtx* Ctx)
{
    if (Ctx == nullptr) {
        return;
    }
    for (;;) {
        KIRQL old;
        KeAcquireSpinLock(&Ctx->AudioIrpLock, &old);
        PIRP irp = DequeueAudioIrp(Ctx, old);
        KeReleaseSpinLock(&Ctx->AudioIrpLock, old);
        if (irp == nullptr) {
            break;
        }
        irp->IoStatus.Status = STATUS_CANCELLED;
        irp->IoStatus.Information = 0;
        IoCompleteRequest(irp, IO_NO_INCREMENT);
    }
    for (;;) {
        KIRQL old;
        KeAcquireSpinLock(&Ctx->EventIrpLock, &old);
        PIRP irp = DequeueEventIrp(Ctx);
        KeReleaseSpinLock(&Ctx->EventIrpLock, old);
        if (irp == nullptr) {
            break;
        }
        irp->IoStatus.Status = STATUS_CANCELLED;
        irp->IoStatus.Information = 0;
        IoCompleteRequest(irp, IO_NO_INCREMENT);
    }
}

/* ------------------------------------------------------------------ */
/* Cancel routines                                                     */
/* ------------------------------------------------------------------ */

static VOID
StreamToSpeakerIoctlCancelAudio(_Inout_ PDEVICE_OBJECT DeviceObject,
                          _Inout_ _IRQL_uses_cancel_ PIRP Irp)
{
    UNREFERENCED_PARAMETER(DeviceObject);
    /* Cancel routine is called with the cancel spinlock held; we must
     * release it ASAP. */
    IoReleaseCancelSpinLock(Irp->CancelIrql);

    /* DeviceObject here is the control device (DeviceExtensionSize=0,
     * StreamToSpeakerGetExt on it returns a garbage pointer → BSOD).
     * Get IoctlCtx from the global publisher instead. */
    StreamToSpeakerIoctlCtx* ctx = StreamToSpeakerCurrentIoctlCtx();
    if (ctx != nullptr) {
        KIRQL old;
        KeAcquireSpinLock(&ctx->AudioIrpLock, &old);
        /* Remove this IRP from the queue if it's still there. */
        if (Irp->Tail.Overlay.ListEntry.Flink != nullptr &&
            Irp->Tail.Overlay.ListEntry.Flink != &Irp->Tail.Overlay.ListEntry) {
            RemoveEntryList(&Irp->Tail.Overlay.ListEntry);
        }
        InitializeListHead(&Irp->Tail.Overlay.ListEntry);
        KeReleaseSpinLock(&ctx->AudioIrpLock, old);
    }

    Irp->IoStatus.Status = STATUS_CANCELLED;
    Irp->IoStatus.Information = 0;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
}

static VOID
StreamToSpeakerIoctlCancelEvent(_Inout_ PDEVICE_OBJECT DeviceObject,
                          _Inout_ _IRQL_uses_cancel_ PIRP Irp)
{
    UNREFERENCED_PARAMETER(DeviceObject);
    IoReleaseCancelSpinLock(Irp->CancelIrql);

    /* See cancel-audio above: never dereference DeviceObject's extension
     * here; the control device has none. */
    StreamToSpeakerIoctlCtx* ctx = StreamToSpeakerCurrentIoctlCtx();
    if (ctx != nullptr) {
        KIRQL old;
        KeAcquireSpinLock(&ctx->EventIrpLock, &old);
        if (Irp->Tail.Overlay.ListEntry.Flink != nullptr &&
            Irp->Tail.Overlay.ListEntry.Flink != &Irp->Tail.Overlay.ListEntry) {
            RemoveEntryList(&Irp->Tail.Overlay.ListEntry);
        }
        InitializeListHead(&Irp->Tail.Overlay.ListEntry);
        KeReleaseSpinLock(&ctx->EventIrpLock, old);
    }

    Irp->IoStatus.Status = STATUS_CANCELLED;
    Irp->IoStatus.Information = 0;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
}
