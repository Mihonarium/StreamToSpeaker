/*
 * driver.cpp - DriverEntry and per-device dispatch glue.
 *
 * The driver registers itself with PortCls (which installs its own
 * dispatch table) and then wraps EVERY major function: IRPs for the
 * PortCls audio FDO go to the saved PortCls routine unchanged, IRPs for
 * our separate control device are handled here and never reach PortCls
 * (which would treat the control device's missing extension as its own).
 *
 * On AddDevice we also:
 *   - allocate the IOCTL context and publish it to the control device
 *   - register the device interface (GUID_DEVINTERFACE_STREAM_TO_SPEAKER)
 *
 * IRP_MJ_PNP on the FDO withdraws the IOCTL context and the topology
 * miniport (waiting for in-flight IOCTLs) before PortCls tears the
 * device down on STOP / SURPRISE_REMOVAL / REMOVE.
 */

/* INITGUID must be defined BEFORE the WDK headers so DEFINE_GUID emits
 * actual storage for the IIDs/CLSIDs (PortCls's IID_IMiniport*, our
 * GUID_DEVINTERFACE_STREAM_TO_SPEAKER, etc.) in this single TU. Any other TU
 * including the same headers gets external declarations and the linker
 * resolves them here. */
#define INITGUID
#include <initguid.h>

#include "driver.h"
#include "ioctl.h"

#ifndef EMULATE_WINDOWS_KERNEL
#include <wdmsec.h>   /* IoCreateDeviceSecure (link wdmsec.lib) */
#endif

#ifndef RTL_CONSTANT_STRING_W
#define RTL_CONSTANT_STRING_W(s)  { sizeof(s) - sizeof(WCHAR), sizeof(s), (PWSTR)(s) }
#endif

static UNICODE_STRING g_SymbolicLink = RTL_CONSTANT_STRING_W(L"\\DosDevices\\StreamToSpeaker");
static UNICODE_STRING g_ControlDeviceName = RTL_CONSTANT_STRING_W(L"\\Device\\StreamToSpeakerCtrl");

/* Control-device security.
 *
 * DACL: SYSTEM and Administrators full control; interactively logged-on
 * users (the service is a per-user GUI app started from the Start menu
 * or the HKCU Run key, not a Windows service) read + write, which is
 * what CreateFileW(GENERIC_READ | GENERIC_WRITE) needs. Network logons,
 * services' own accounts, restricted tokens and AppContainers get no
 * access. FILE_DEVICE_SECURE_OPEN applies the same check to opens of
 * any name below the device.
 *
 * Integrity: IoCreateDeviceSecure accepts a DACL only (no S: mandatory
 * label). An unlabeled device object is treated as Medium integrity
 * with no-write-up, so a Low-integrity caller can open it for read but
 * never for write. The CREATE handler therefore requires every
 * user-mode open to ask for write access; together that keeps
 * Low-integrity (sandboxed) processes from opening the device at all,
 * including read-only opens that could otherwise pull audio packets.
 *
 * The class GUID names the registry key under which an administrator
 * can override this descriptor (Class\{GUID}\Properties). */
static const UNICODE_STRING g_ControlDeviceSddl = RTL_CONSTANT_STRING_W(
    L"D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");

/* {C82EC36C-B4B0-4DB5-BB28-80E5EE94D83C} */
DEFINE_GUID(GUID_DEVCLASS_STREAM_TO_SPEAKER_CONTROL,
    0xc82ec36c, 0xb4b0, 0x4db5, 0xbb, 0x28, 0x80, 0xe5, 0xee, 0x94, 0xd8, 0x3c);

/* Separate "control" device. Used solely for user-mode CreateFileW
 * and our custom IOCTLs. It's NOT part of the KS audio stack —
 * it is created as a non-PnP NT device, so ksthunk and
 * PortCls's KS validation never see opens on it.
 *
 * The PortCls FDO continues to own the audio rendering path. The
 * dispatch wrappers below branch on DeviceObject to route IRPs:
 *   - Audio FDO   → PortCls (saved dispatch table)
 *   - Control DO  → our handlers, talking to g_IoctlCtx
 *
 * g_IoctlCtx is shared: the WaveRT capture DPC writes into it, the
 * IOCTL handler drains it. It's allocated in AddDevice (when the
 * audio FDO comes up) and a pointer published here for the control
 * device to find. */
static PDEVICE_OBJECT          g_ControlDevice = nullptr;
static volatile PVOID          g_IoctlCtxPtr = nullptr;
static BOOLEAN                 g_ControlSymlinkCreated = FALSE;

/* Guards g_IoctlCtxPtr's target: the control-device dispatchers acquire
 * it before reading the pointer, and each IRP they queue owns one more
 * reference. Withdrawal clears the pointer, cancels the queue and waits
 * for the rundown before the context (and the FDO it points into) can
 * be freed. Lives in the driver image, so it outlives every context. */
static EX_RUNDOWN_REF          g_IoctlCtxRundown;

static PDRIVER_ADD_DEVICE      g_PortClsAddDevice = nullptr;
static PDRIVER_DISPATCH        g_PortClsDispatchTable[IRP_MJ_MAXIMUM_FUNCTION + 1];

extern "C" BOOLEAN StreamToSpeakerIoctlRefAcquire()
{
    return ExAcquireRundownProtection(&g_IoctlCtxRundown);
}

extern "C" VOID StreamToSpeakerIoctlRefRelease()
{
    ExReleaseRundownProtection(&g_IoctlCtxRundown);
}

/* Publish ctx to the control device. The driver serves one virtual
 * device: if a context is already published, the new one stays private
 * (no IOCTL can reach it) rather than silently replacing the other. */
static BOOLEAN PublishIoctlCtx(StreamToSpeakerIoctlCtx* ctx)
{
    return InterlockedCompareExchangePointer(
        (PVOID volatile *)&g_IoctlCtxPtr, (PVOID)ctx, nullptr) == nullptr;
}

/* Make ext's IOCTL context unreachable and quiescent: clear the global
 * pointer, refuse further queueing, complete every queued IRP and wait
 * until no dispatcher or queued IRP references it. Afterwards only the
 * audio-side producers (WaveRT DPC, topology setters) may still touch
 * it, which is why the memory is freed only on REMOVE. PASSIVE_LEVEL. */
static VOID WithdrawIoctlCtx(PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext)
{
    PAGED_CODE();
    if (ext->IoctlCtx == nullptr || ext->IoctlCtxWithdrawn) {
        return;
    }
    ext->IoctlCtxWithdrawn = TRUE;

    BOOLEAN wasPublished =
        InterlockedCompareExchangePointer(
            (PVOID volatile *)&g_IoctlCtxPtr, nullptr, (PVOID)ext->IoctlCtx)
        == (PVOID)ext->IoctlCtx;

    IoctlBeginClose(ext->IoctlCtx);

    if (wasPublished) {
        ExWaitForRundownProtectionRelease(&g_IoctlCtxRundown);
        /* Re-arm for a device added later; with the pointer cleared a
         * new reference can only observe nullptr. */
        ExReInitializeRundownProtection(&g_IoctlCtxRundown);
    }
}

static VOID DestroyIoctlCtx(PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext)
{
    PAGED_CODE();
    if (ext->IoctlCtx == nullptr) {
        return;
    }
    WithdrawIoctlCtx(ext);
    StreamToSpeakerIoctlCtx* ctx = ext->IoctlCtx;
    ext->IoctlCtx = nullptr;
    IoctlCtxDestroy(ctx);
    ExFreePoolWithTag(ctx, STREAM_TO_SPEAKER_POOL_TAG);
}

static VOID DisableDeviceInterface(PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext,
                                   BOOLEAN freeLink)
{
    PAGED_CODE();
    if (ext->DeviceInterfaceRegistered) {
        IoSetDeviceInterfaceState(&ext->DeviceInterfaceLink, FALSE);
        ext->DeviceInterfaceRegistered = FALSE;
    }
    if (freeLink && ext->DeviceInterfaceLink.Buffer != nullptr) {
        RtlFreeUnicodeString(&ext->DeviceInterfaceLink);
        RtlInitUnicodeString(&ext->DeviceInterfaceLink, nullptr);
    }
}

/* Externally callable (e.g. from ioctl.cpp cancel routines) so any code
 * holding only a control-device DeviceObject can still find the IoctlCtx.
 * The control device has DeviceExtensionSize=0, so traversing it via
 * StreamToSpeakerGetExt is a use-after-free / use-of-garbage and BSODs. */
extern "C" StreamToSpeakerIoctlCtx* StreamToSpeakerCurrentIoctlCtx()
{
    return (StreamToSpeakerIoctlCtx*)InterlockedCompareExchangePointer(
        (PVOID volatile *)&g_IoctlCtxPtr, nullptr, nullptr);
}

static StreamToSpeakerIoctlCtx* CurrentIoctlCtx()
{
    return StreamToSpeakerCurrentIoctlCtx();
}

static NTSTATUS CompleteControlIrp(_In_ PIRP Irp, _In_ NTSTATUS Status)
{
    Irp->IoStatus.Status = Status;
    Irp->IoStatus.Information = 0;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
    return Status;
}

/* User-mode opens of the control device must request write access. See
 * g_ControlDeviceSddl for why: the I/O manager has already checked the
 * requested access against the DACL and the implicit Medium no-write-up
 * label, so requiring write here excludes Low-integrity callers even
 * when they ask for read only. MAXIMUM_ALLOWED on its own is refused
 * for the same reason. OriginalDesiredAccess may hold mapped or
 * unmapped generic rights, so accept either form. */
static NTSTATUS ControlDeviceCheckOpen(_In_ PIRP Irp)
{
    PAGED_CODE();
    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    if (Irp->RequestorMode == KernelMode &&
        (sp->Flags & SL_FORCE_ACCESS_CHECK) == 0) {
        return STATUS_SUCCESS;
    }
    PIO_SECURITY_CONTEXT sc = sp->Parameters.Create.SecurityContext;
    if (sc == nullptr || sc->AccessState == nullptr) {
        return STATUS_ACCESS_DENIED;
    }
    const ACCESS_MASK writeRights = FILE_WRITE_DATA | GENERIC_WRITE | GENERIC_ALL;
    if ((sc->AccessState->OriginalDesiredAccess & writeRights) == 0) {
        return STATUS_ACCESS_DENIED;
    }
    return STATUS_SUCCESS;
}

extern "C" NTSTATUS
StreamToSpeakerDispatchCreateClose(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    PAGED_CODE();

    /* Route by DeviceObject: control device → handled here; audio FDO → PortCls. */
    if (DeviceObject == g_ControlDevice) {
        PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
        NTSTATUS status = STATUS_SUCCESS;
        if (sp->MajorFunction == IRP_MJ_CREATE) {
            status = ControlDeviceCheckOpen(Irp);
        }
        return CompleteControlIrp(Irp, status);
    }
    return StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
}

/* IRP_MJ_CLEANUP: the last handle to a file object is gone. Complete
 * the IRPs it still has queued instead of leaving them pending until
 * the issuing thread exits. */
extern "C" NTSTATUS
StreamToSpeakerDispatchCleanup(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    PAGED_CODE();
    if (DeviceObject != g_ControlDevice) {
        return StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
    }
    if (StreamToSpeakerIoctlRefAcquire()) {
        StreamToSpeakerIoctlCtx* ctx = CurrentIoctlCtx();
        if (ctx != nullptr) {
            IoctlCancelFileIrps(ctx, IoGetCurrentIrpStackLocation(Irp)->FileObject);
        }
        StreamToSpeakerIoctlRefRelease();
    }
    /* No context: it was withdrawn, which already completed its IRPs. */
    return CompleteControlIrp(Irp, STATUS_SUCCESS);
}

/* Every major function without a dedicated handler (READ, WRITE,
 * FLUSH_BUFFERS, QUERY/SET_SECURITY, INTERNAL_DEVICE_CONTROL, ...).
 * PortCls/KS dispatch these through the file object's KS object header,
 * which control-device file objects do not have, so they must never be
 * forwarded for the control device. */
extern "C" NTSTATUS
StreamToSpeakerDispatchOther(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    if (DeviceObject == g_ControlDevice) {
        return CompleteControlIrp(Irp, STATUS_INVALID_DEVICE_REQUEST);
    }
    return StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
}

extern "C" NTSTATUS
StreamToSpeakerDispatchPassThrough(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    PDRIVER_DISPATCH fn = g_PortClsDispatchTable[sp->MajorFunction];
    if (fn != nullptr) {
        return fn(DeviceObject, Irp);
    }
    Irp->IoStatus.Status = STATUS_NOT_SUPPORTED;
    Irp->IoStatus.Information = 0;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
    return STATUS_NOT_SUPPORTED;
}

/* IRP_MJ_POWER dispatcher.
 *
 * Power IRPs go to every device object the driver owns — that
 * includes our control device (created via IoCreateDevice with
 * DeviceExtensionSize=0), not just the PortCls-created audio FDO.
 * PortCls's Power handler only knows about the audio FDO; when it
 * sees a Power IRP for our control device it dereferences a
 * non-existent device extension into garbage and either stalls or
 * skips PoStartNextPowerIrp / completion, after which the system
 * hits BugCheck 0x9F (DRIVER_POWER_STATE_FAILURE) sub-code 3
 * ("a device object has been blocking an IRP for too long").
 *
 * Route by device:
 *   - Control device: complete with success. There's no power
 *     state to manage; we just need to acknowledge so the power
 *     manager moves on.
 *   - Audio FDO: forward to PortCls.
 */
extern "C" NTSTATUS
StreamToSpeakerDispatchPower(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    if (DeviceObject == g_ControlDevice) {
        Irp->IoStatus.Status = STATUS_SUCCESS;
        Irp->IoStatus.Information = 0;
        /* PoStartNextPowerIrp is deprecated post-Vista but harmless
         * on modern Windows. Calling it costs nothing and keeps
         * older WHQL test runners quiet. */
        PoStartNextPowerIrp(Irp);
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return STATUS_SUCCESS;
    }
    /* Audio FDO — let PortCls do its thing. */
    return StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
}

/* IRP_MJ_SYSTEM_CONTROL (WMI) is the other major that PortCls
 * doesn't know to handle on our non-audio control device. Same
 * routing pattern. */
extern "C" NTSTATUS
StreamToSpeakerDispatchSystemControl(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    if (DeviceObject == g_ControlDevice) {
        NTSTATUS status = STATUS_NOT_SUPPORTED;
        Irp->IoStatus.Status = status;
        Irp->IoStatus.Information = 0;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return status;
    }
    return StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
}

extern "C" NTSTATUS
StreamToSpeakerDispatchDeviceControl(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    PAGED_CODE();
    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    ULONG ioctl = sp->Parameters.DeviceIoControl.IoControlCode;

    /* DEVICE_CONTROL on the audio FDO is always a PortCls/KS IOCTL —
     * forward without inspection. Our IOCTLs only arrive on the control
     * device, so we don't even need to check IoControlCode there. */
    if (DeviceObject != g_ControlDevice) {
        return StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
    }

    /* Recognise our IOCTLs explicitly; unknown ones on the control
     * device get NOT_SUPPORTED rather than being silently passed. */
    BOOLEAN ours =
        (ioctl == IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET) ||
        (ioctl == IOCTL_STREAM_TO_SPEAKER_GET_CONTROL_EVENT) ||
        (ioctl == IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME) ||
        (ioctl == IOCTL_STREAM_TO_SPEAKER_GET_VERSION);
    if (!ours) {
        Irp->IoStatus.Status = STATUS_INVALID_DEVICE_REQUEST;
        Irp->IoStatus.Information = 0;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return STATUS_INVALID_DEVICE_REQUEST;
    }

    /* Hold the context reference for the whole dispatch: the context
     * and the FDO it points at stay valid until we release it. */
    if (!StreamToSpeakerIoctlRefAcquire()) {
        /* Device removal in progress. */
        return CompleteControlIrp(Irp, STATUS_DEVICE_NOT_READY);
    }
    StreamToSpeakerIoctlCtx* ctx = CurrentIoctlCtx();
    if (ctx == nullptr) {
        /* No audio FDO (not added yet, or removed). Tell the caller to
         * try again. */
        StreamToSpeakerIoctlRefRelease();
        return CompleteControlIrp(Irp, STATUS_DEVICE_NOT_READY);
    }

    NTSTATUS status = IoctlDispatch(ctx, Irp);
    if (status != STATUS_PENDING) {
        Irp->IoStatus.Status = status;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
    }
    /* A pended IRP holds its own reference (see ioctl.cpp). */
    StreamToSpeakerIoctlRefRelease();
    return status;
}

/* IRP_MJ_PNP.
 *
 * Control device: it is a non-PnP device object, so the only PnP IRPs
 * it can see are queries such as the TargetDeviceRelation query sent
 * when someone registers for handle notifications. Complete them with
 * the status already in the IRP (the PnP convention for IRPs a driver
 * does not handle) instead of letting PortCls read the extension the
 * control device does not have.
 *
 * Audio FDO: do our part BEFORE PortCls, because PortCls tears down the
 * subdevices on STOP and detaches and deletes the FDO on REMOVE — after
 * forwarding REMOVE the extension is gone and must not be touched.
 *   STOP_DEVICE       withdraw the topology miniport (PortCls is about
 *                     to release it); the IOCTL context stays.
 *   SURPRISE_REMOVAL  withdraw the topology and the IOCTL context (no
 *                     new IOCTLs, queued IRPs completed, in-flight ones
 *                     drained) and disable the device interface. Audio
 *                     streams may still be open, so nothing is freed.
 *   REMOVE_DEVICE     the same if surprise removal did not happen, then
 *                     free the context. All handles to the FDO are
 *                     closed by now, so no stream or topology callback
 *                     can still reach it.
 */
extern "C" NTSTATUS
StreamToSpeakerDispatchPnp(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    PAGED_CODE();
    if (DeviceObject == g_ControlDevice) {
        NTSTATUS status = Irp->IoStatus.Status;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return status;
    }

    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext = StreamToSpeakerGetExt(DeviceObject);
    if (ext != nullptr) {
        switch (sp->MinorFunction) {
        case IRP_MN_STOP_DEVICE:
            TopologyWithdraw(ext);
            break;

        case IRP_MN_SURPRISE_REMOVAL:
            TopologyWithdraw(ext);
            WithdrawIoctlCtx(ext);
            DisableDeviceInterface(ext, FALSE);
            break;

        case IRP_MN_REMOVE_DEVICE:
            TopologyWithdraw(ext);
            DestroyIoctlCtx(ext);
            DisableDeviceInterface(ext, TRUE);
            break;

        default:
            break;
        }
    }

    return StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
}

/* StreamToSpeakerAddDeviceInit — called from adapter.cpp's StreamToSpeakerAddDevice
 * after PcAddAdapterDevice returns success. Performs the per-device
 * setup that previously lived in StreamToSpeakerAddDeviceWrapper. */
extern "C" NTSTATUS
StreamToSpeakerAddDeviceInit(
    _In_ PDRIVER_OBJECT  DriverObject,
    _In_ PDEVICE_OBJECT  PhysicalDeviceObject)
{
    PAGED_CODE();

    PDEVICE_OBJECT match = DriverObject->DeviceObject;
    if (match == nullptr) {
        DBG_ERROR("StreamToSpeakerAddDeviceInit: no FDO created");
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }

    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext =
        StreamToSpeakerGetExt(match);
    if (ext == nullptr) {
        DBG_ERROR("StreamToSpeakerAddDeviceInit: ext is null");
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }

    ext->DeviceObject              = match;
    ext->PhysicalDeviceObject      = PhysicalDeviceObject;
    ext->DeviceInterfaceRegistered = FALSE;
    ext->SymbolicLinkCreated       = FALSE;
    ext->IoctlCtxWithdrawn         = FALSE;
    ext->Topology                  = nullptr;
    RtlInitUnicodeString(&ext->DeviceInterfaceLink, nullptr);
    ExInitializeRundownProtection(&ext->TopologyRundown);

    /* IOCTL context. */
    ext->IoctlCtx = static_cast<StreamToSpeakerIoctlCtx*>(
        ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(StreamToSpeakerIoctlCtx),
                        STREAM_TO_SPEAKER_POOL_TAG));
    if (ext->IoctlCtx == nullptr) {
        DBG_ERROR("IoctlCtx allocation failed");
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    NTSTATUS status = IoctlCtxInit(ext->IoctlCtx, match);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("IoctlCtxInit failed 0x%08x", status);
        ExFreePoolWithTag(ext->IoctlCtx, STREAM_TO_SPEAKER_POOL_TAG);
        ext->IoctlCtx = nullptr;
        return status;
    }

    /* Publish the IoctlCtx so the separate control device's IOCTL
     * dispatcher can find it. (See driver.cpp top for rationale.) */
    if (!PublishIoctlCtx(ext->IoctlCtx)) {
        DBG_WARN("another device already serves the control device; "
                 "this one is not reachable through IOCTLs");
    }

    /* Register a device interface with a reference string. The reference
     * string serves as a discriminator: user-mode CreateFileW on the path
     * returned by SetupDi puts the reference in FileObject->FileName, and
     * our CREATE dispatcher routes by matching that string. Without a
     * reference string ksthunk/PortCls reject the open with
     * STATUS_OBJECT_NAME_NOT_FOUND (the cause of ERROR_FILE_NOT_FOUND we
     * see when opening the interface path bare). Pattern is SAR-derived. */
    {
        UNICODE_STRING ref;
        RtlInitUnicodeString(&ref, STREAM_TO_SPEAKER_CONTROL_REF_STRING);
        status = IoRegisterDeviceInterface(
            PhysicalDeviceObject,
            &GUID_DEVINTERFACE_STREAM_TO_SPEAKER,
            &ref,
            &ext->DeviceInterfaceLink);
        if (NT_SUCCESS(status)) {
            ext->DeviceInterfaceRegistered = TRUE;
            IoSetDeviceInterfaceState(&ext->DeviceInterfaceLink, TRUE);
            DBG_INFO("device interface registered with reference '%wZ'", &ext->DeviceInterfaceLink);
        } else {
            DBG_WARN("IoRegisterDeviceInterface failed 0x%08x (non-fatal)", status);
        }
    }

    /* No symbolic link: PortCls's FDO has an anonymous name
     * (\Device\NNNNNNNN), so \DosDevices\StreamToSpeaker → \Device\StreamToSpeaker
     * resolves to a non-existent NT name and CreateFileW would return
     * ERROR_FILE_NOT_FOUND. User-mode goes through the device interface
     * exclusively. */
    ext->SymbolicLinkCreated = FALSE;

    /* DO_DIRECT_IO for METHOD_OUT_DIRECT IOCTLs. PortCls already
     * cleared DO_DEVICE_INITIALIZING. */
    match->Flags |= DO_DIRECT_IO;

    DBG_INFO("StreamToSpeakerAddDeviceInit complete");
    return STATUS_SUCCESS;
}

extern "C" VOID
StreamToSpeakerDriverUnload(_In_ PDRIVER_OBJECT DriverObject)
{
    PAGED_CODE();
    /* PnP unloads the driver only after every FDO has been removed, so
     * the loop below normally finds nothing; it is a safety net. */
    PDEVICE_OBJECT control = g_ControlDevice;
    for (PDEVICE_OBJECT d = DriverObject->DeviceObject; d != nullptr; d = d->NextDevice) {
        if (d == control) {
            continue;  /* no extension */
        }
        PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext =
            StreamToSpeakerGetExt(d);
        if (ext == nullptr) {
            continue;
        }
        DestroyIoctlCtx(ext);
        DisableDeviceInterface(ext, TRUE);
    }

    if (g_ControlSymlinkCreated) {
        IoDeleteSymbolicLink(&g_SymbolicLink);
        g_ControlSymlinkCreated = FALSE;
    }
    if (control != nullptr) {
        /* g_ControlDevice keeps naming it: CLEANUP / CLOSE for handles
         * still open must keep routing to our handlers, not PortCls. */
        IoDeleteDevice(control);
    }
}

extern "C" NTSTATUS
DriverEntry(
    _In_ PDRIVER_OBJECT  DriverObject,
    _In_ PUNICODE_STRING RegistryPath)
{
    PAGED_CODE();
    ExInitializeRundownProtection(&g_IoctlCtxRundown);

    NTSTATUS status = PcInitializeAdapterDriver(
        DriverObject, RegistryPath, StreamToSpeakerAddDevice);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("PcInitializeAdapterDriver failed 0x%08x", status);
        return status;
    }

    /* Save PortCls's dispatch routines and override the ones we care
     * about. */
    for (ULONG i = 0; i <= IRP_MJ_MAXIMUM_FUNCTION; ++i) {
        g_PortClsDispatchTable[i] = DriverObject->MajorFunction[i];
    }

    /* Every major gets a wrapper that routes by device object: the
     * audio FDO always reaches the saved PortCls routine, the control
     * device never does. Majors without a dedicated handler complete
     * with STATUS_INVALID_DEVICE_REQUEST on the control device. */
    for (ULONG i = 0; i <= IRP_MJ_MAXIMUM_FUNCTION; ++i) {
        DriverObject->MajorFunction[i] = StreamToSpeakerDispatchOther;
    }
    /* DEVICE_CONTROL: our IOCTLs on the control device; KS/streaming
     * IOCTLs on the FDO pass through. CREATE / CLOSE: opens of the
     * control device are completed here (PortCls's CREATE would reject
     * them for lacking a KS object name). */
    DriverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL] = StreamToSpeakerDispatchDeviceControl;
    DriverObject->MajorFunction[IRP_MJ_CREATE]         = StreamToSpeakerDispatchCreateClose;
    DriverObject->MajorFunction[IRP_MJ_CLOSE]          = StreamToSpeakerDispatchCreateClose;
    DriverObject->MajorFunction[IRP_MJ_CLEANUP]        = StreamToSpeakerDispatchCleanup;
    /* See StreamToSpeakerDispatchPower for the BugCheck 0x9F rationale. */
    DriverObject->MajorFunction[IRP_MJ_POWER]          = StreamToSpeakerDispatchPower;
    DriverObject->MajorFunction[IRP_MJ_SYSTEM_CONTROL] = StreamToSpeakerDispatchSystemControl;
    DriverObject->MajorFunction[IRP_MJ_PNP]            = StreamToSpeakerDispatchPnp;

    DriverObject->DriverUnload = StreamToSpeakerDriverUnload;
    g_PortClsAddDevice = DriverObject->DriverExtension->AddDevice;

    /* ---------------------------------------------------------------
     * Create the separate control device.
     *
     * IoCreateDeviceSecure with FILE_DEVICE_UNKNOWN gives us a plain NT
     * device (security: see g_ControlDeviceSddl) that is NOT enrolled
     * in the PnP audio stack. ksthunk and
     * PortCls's KS validation never look at it, so user-mode
     * CreateFileW("\\.\StreamToSpeaker") routes straight to our
     * dispatch handlers via the symbolic link.
     * --------------------------------------------------------------- */
    /* Pre-emptively delete any leftover symlink from a previous load.
     * NT device objects and DosDevices symlinks survive across driver
     * reinstall within a single boot session — without this, the second
     * load returns STATUS_OBJECT_NAME_COLLISION (Code 37). */
    IoDeleteSymbolicLink(&g_SymbolicLink);  /* errors ignored — link may not exist */

    NTSTATUS cdStatus = IoCreateDeviceSecure(
        DriverObject,
        0,                          /* no device-extension; we keep state globally */
        &g_ControlDeviceName,
        FILE_DEVICE_UNKNOWN,
        FILE_DEVICE_SECURE_OPEN,    /* apply the DACL to opens below the device name too */
        FALSE,                      /* not exclusive — multiple opens OK */
        &g_ControlDeviceSddl,
        &GUID_DEVCLASS_STREAM_TO_SPEAKER_CONTROL,
        &g_ControlDevice);
    if (cdStatus == STATUS_OBJECT_NAME_COLLISION) {
        /* Device object survived a previous load. Without a handle we
         * can't delete it directly. Best we can do is reuse the name
         * by picking a fresh one with an instance counter. */
        DBG_WARN("control device name collision; reboot to fully clear, retrying with suffix");
        UNICODE_STRING altName;
        altName.MaximumLength = 128;
        altName.Length = 0;
        altName.Buffer = static_cast<PWSTR>(ExAllocatePool2(
            POOL_FLAG_PAGED, altName.MaximumLength, STREAM_TO_SPEAKER_POOL_TAG));
        if (altName.Buffer != nullptr) {
            ULONG tick = (ULONG)(KeQueryInterruptTime() & 0xFFFF);
            UNICODE_STRING base = g_ControlDeviceName;
            RtlCopyUnicodeString(&altName, &base);
            WCHAR suffix[8];
            for (int i = 0; i < 4; i++) {
                WCHAR nib = (WCHAR)((tick >> (i * 4)) & 0xF);
                suffix[i] = (nib < 10) ? (WCHAR)(L'0' + nib) : (WCHAR)(L'A' + nib - 10);
            }
            suffix[4] = 0;
            UNICODE_STRING suff;
            RtlInitUnicodeString(&suff, suffix);
            RtlAppendUnicodeStringToString(&altName, &suff);
            cdStatus = IoCreateDeviceSecure(
                DriverObject, 0, &altName,
                FILE_DEVICE_UNKNOWN, FILE_DEVICE_SECURE_OPEN, FALSE,
                &g_ControlDeviceSddl,
                &GUID_DEVCLASS_STREAM_TO_SPEAKER_CONTROL,
                &g_ControlDevice);
            ExFreePoolWithTag(altName.Buffer, STREAM_TO_SPEAKER_POOL_TAG);
        }
    }
    if (!NT_SUCCESS(cdStatus)) {
        DBG_ERROR("IoCreateDevice(control) failed 0x%08x", cdStatus);
        return cdStatus;
    }
    g_ControlDevice->Flags |= DO_DIRECT_IO;       /* METHOD_OUT_DIRECT for audio IOCTL */
    g_ControlDevice->Flags &= ~DO_DEVICE_INITIALIZING;

    /* Symlink: after the IoDeleteSymbolicLink above, this should succeed.
     * If we still hit a collision, force-delete and retry. */
    NTSTATUS slStatus = IoCreateSymbolicLink(&g_SymbolicLink, &g_ControlDeviceName);
    if (slStatus == STATUS_OBJECT_NAME_COLLISION) {
        IoDeleteSymbolicLink(&g_SymbolicLink);
        slStatus = IoCreateSymbolicLink(&g_SymbolicLink, &g_ControlDeviceName);
    }
    if (NT_SUCCESS(slStatus)) {
        g_ControlSymlinkCreated = TRUE;
        DBG_INFO("control device created: %wZ  symlink: %wZ",
                 &g_ControlDeviceName, &g_SymbolicLink);
    } else {
        DBG_ERROR("IoCreateSymbolicLink failed 0x%08x — control device unreachable",
                  slStatus);
        IoDeleteDevice(g_ControlDevice);
        g_ControlDevice = nullptr;
        return slStatus;
    }

    DBG_INFO("Stream To Speaker driver loaded, build %u, proto %u",
             STREAM_TO_SPEAKER_DRIVER_BUILD, STREAM_TO_SPEAKER_PROTOCOL_VERSION);
    return STATUS_SUCCESS;
}
