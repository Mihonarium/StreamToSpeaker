/*
 * driver.cpp - DriverEntry and per-device dispatch glue.
 *
 * The driver registers itself with PortCls (which installs its own
 * dispatch table) and then *overrides* IRP_MJ_DEVICE_CONTROL,
 * IRP_MJ_CREATE, and IRP_MJ_CLOSE with our own handlers so the
 * user-mode service can talk to us via DeviceIoControl. All other
 * majors fall through to PortCls.
 *
 * On AddDevice we also:
 *   - allocate the IOCTL context
 *   - register the device interface (GUID_DEVINTERFACE_STREAM_TO_SPEAKER)
 *   - create a symbolic link \DosDevices\StreamToSpeaker
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

#ifndef RTL_CONSTANT_STRING_W
#define RTL_CONSTANT_STRING_W(s)  { sizeof(s) - sizeof(WCHAR), sizeof(s), (PWSTR)(s) }
#endif

static UNICODE_STRING g_SymbolicLink = RTL_CONSTANT_STRING_W(L"\\DosDevices\\StreamToSpeaker");
static UNICODE_STRING g_ControlDeviceName = RTL_CONSTANT_STRING_W(L"\\Device\\StreamToSpeakerCtrl");

/* Separate "control" device. Used solely for user-mode CreateFileW
 * and our custom IOCTLs. It's NOT part of the KS audio stack —
 * IoCreateDevice creates it as a non-PnP NT device, so ksthunk and
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

static PDRIVER_ADD_DEVICE      g_PortClsAddDevice = nullptr;
static PDRIVER_DISPATCH        g_PortClsDispatchTable[IRP_MJ_MAXIMUM_FUNCTION + 1];

/* Helper used by the audio-side code to publish/withdraw its IoctlCtx
 * pointer so the control device can dispatch IOCTLs against it. */
extern "C" VOID StreamToSpeakerPublishIoctlCtx(StreamToSpeakerIoctlCtx* ctx)
{
    InterlockedExchangePointer((PVOID volatile *)&g_IoctlCtxPtr, (PVOID)ctx);
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

extern "C" NTSTATUS
StreamToSpeakerDispatchCreateClose(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    PAGED_CODE();

    /* Route by DeviceObject: control device → succeed; audio FDO → PortCls. */
    if (DeviceObject == g_ControlDevice) {
        Irp->IoStatus.Status = STATUS_SUCCESS;
        Irp->IoStatus.Information = 0;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return STATUS_SUCCESS;
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

    StreamToSpeakerIoctlCtx* ctx = CurrentIoctlCtx();
    if (ctx == nullptr) {
        /* No audio FDO yet (driver loaded, but PnP hasn't called AddDevice).
         * Tell the caller to try again. */
        Irp->IoStatus.Status = STATUS_DEVICE_NOT_READY;
        Irp->IoStatus.Information = 0;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return STATUS_DEVICE_NOT_READY;
    }

    NTSTATUS status = IoctlDispatch(ctx, Irp);
    if (status != STATUS_PENDING) {
        Irp->IoStatus.Status = status;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
    }
    return status;
}

/* PnP IRP_MJ_PNP: handle device-interface registration on
 * IRP_MN_START_DEVICE and tear-down on REMOVE/STOP. We delegate the
 * actual IRP to PortCls. */
extern "C" NTSTATUS
StreamToSpeakerDispatchPnp(_In_ PDEVICE_OBJECT DeviceObject, _In_ PIRP Irp)
{
    PAGED_CODE();
    PIO_STACK_LOCATION sp = IoGetCurrentIrpStackLocation(Irp);
    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext =
        StreamToSpeakerGetExt(DeviceObject);

    NTSTATUS status = StreamToSpeakerDispatchPassThrough(DeviceObject, Irp);
    if (!NT_SUCCESS(status) || ext == nullptr) {
        return status;
    }

    switch (sp->MinorFunction) {
    case IRP_MN_START_DEVICE:
        if (!ext->DeviceInterfaceRegistered) {
            NTSTATUS s = IoRegisterDeviceInterface(
                ext->PhysicalDeviceObject,
                &GUID_DEVINTERFACE_STREAM_TO_SPEAKER,
                nullptr,
                &ext->DeviceInterfaceLink);
            if (NT_SUCCESS(s)) {
                ext->DeviceInterfaceRegistered = TRUE;
                IoSetDeviceInterfaceState(&ext->DeviceInterfaceLink, TRUE);
            } else {
                DBG_WARN("IoRegisterDeviceInterface failed 0x%08x", s);
            }
        }
        if (!ext->SymbolicLinkCreated) {
            /* Symbolic link points at the device by name. We have to
             * give the device a name first via IoCreateSymbolicLink
             * against the PDO's NT name. The simplest path: just
             * advertise the link as a Win32 alias of the device
             * interface symlink we registered above. */
            UNICODE_STRING devName;
            RtlInitUnicodeString(&devName, L"\\Device\\StreamToSpeaker");
            NTSTATUS s = IoCreateSymbolicLink(&g_SymbolicLink, &devName);
            if (NT_SUCCESS(s)) {
                ext->SymbolicLinkCreated = TRUE;
            } else {
                DBG_WARN("IoCreateSymbolicLink failed 0x%08x (non-fatal)", s);
            }
        }
        break;

    case IRP_MN_REMOVE_DEVICE:
    case IRP_MN_SURPRISE_REMOVAL:
        if (ext->DeviceInterfaceRegistered) {
            IoSetDeviceInterfaceState(&ext->DeviceInterfaceLink, FALSE);
            RtlFreeUnicodeString(&ext->DeviceInterfaceLink);
            RtlInitUnicodeString(&ext->DeviceInterfaceLink, nullptr);
            ext->DeviceInterfaceRegistered = FALSE;
        }
        if (ext->SymbolicLinkCreated) {
            IoDeleteSymbolicLink(&g_SymbolicLink);
            ext->SymbolicLinkCreated = FALSE;
        }
        if (ext->IoctlCtx != nullptr) {
            IoctlCancelAll(ext->IoctlCtx);
            IoctlCtxDestroy(ext->IoctlCtx);
            ExFreePoolWithTag(ext->IoctlCtx, STREAM_TO_SPEAKER_POOL_TAG);
            ext->IoctlCtx = nullptr;
        }
        break;

    default:
        break;
    }

    return status;
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
    RtlInitUnicodeString(&ext->DeviceInterfaceLink, nullptr);

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
    StreamToSpeakerPublishIoctlCtx(ext->IoctlCtx);

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
    /* Tear down the control device first so no new IOCTLs can come in. */
    StreamToSpeakerPublishIoctlCtx(nullptr);
    if (g_ControlSymlinkCreated) {
        IoDeleteSymbolicLink(&g_SymbolicLink);
        g_ControlSymlinkCreated = FALSE;
    }
    if (g_ControlDevice != nullptr) {
        IoDeleteDevice(g_ControlDevice);
        g_ControlDevice = nullptr;
    }

    /* Walk and free our state on each audio FDO. PortCls handles
     * device-object teardown for us; we only release our extension. */
    for (PDEVICE_OBJECT d = DriverObject->DeviceObject; d != nullptr; d = d->NextDevice) {
        if (d == g_ControlDevice) {
            continue;  /* already deleted */
        }
        PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext =
            StreamToSpeakerGetExt(d);
        if (ext == nullptr) {
            continue;
        }
        if (ext->IoctlCtx != nullptr) {
            IoctlCancelAll(ext->IoctlCtx);
            IoctlCtxDestroy(ext->IoctlCtx);
            ExFreePoolWithTag(ext->IoctlCtx, STREAM_TO_SPEAKER_POOL_TAG);
            ext->IoctlCtx = nullptr;
        }
        if (ext->DeviceInterfaceRegistered) {
            IoSetDeviceInterfaceState(&ext->DeviceInterfaceLink, FALSE);
            RtlFreeUnicodeString(&ext->DeviceInterfaceLink);
            ext->DeviceInterfaceRegistered = FALSE;
        }
    }
}

extern "C" NTSTATUS
DriverEntry(
    _In_ PDRIVER_OBJECT  DriverObject,
    _In_ PUNICODE_STRING RegistryPath)
{
    PAGED_CODE();
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

    /* Override DEVICE_CONTROL so our IOCTLs reach our handler; KS/streaming
     * IOCTLs fall through via StreamToSpeakerDispatchPassThrough.
     * Override CREATE and CLOSE so user-mode CreateFileW("\\.\StreamToSpeaker"
     * or the device-interface symbolic-link) succeeds — without this, PortCls's
     * default CREATE handler rejects opens that lack a KS reference string
     * and the user-mode service can't connect. Our handler delegates KS opens
     * (non-empty FileName) back to PortCls. */
    DriverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL] = StreamToSpeakerDispatchDeviceControl;
    DriverObject->MajorFunction[IRP_MJ_CREATE]         = StreamToSpeakerDispatchCreateClose;
    DriverObject->MajorFunction[IRP_MJ_CLOSE]          = StreamToSpeakerDispatchCreateClose;
    /* See StreamToSpeakerDispatchPower for the BugCheck 0x9F rationale. */
    DriverObject->MajorFunction[IRP_MJ_POWER]          = StreamToSpeakerDispatchPower;
    DriverObject->MajorFunction[IRP_MJ_SYSTEM_CONTROL] = StreamToSpeakerDispatchSystemControl;

    DriverObject->DriverUnload = StreamToSpeakerDriverUnload;
    g_PortClsAddDevice = DriverObject->DriverExtension->AddDevice;

    /* ---------------------------------------------------------------
     * Create the separate control device.
     *
     * IoCreateDevice with FILE_DEVICE_UNKNOWN gives us a plain NT
     * device that is NOT enrolled in the PnP audio stack. ksthunk and
     * PortCls's KS validation never look at it, so user-mode
     * CreateFileW("\\.\StreamToSpeaker") routes straight to our
     * dispatch handlers via the symbolic link.
     * --------------------------------------------------------------- */
    /* Pre-emptively delete any leftover symlink from a previous load.
     * NT device objects and DosDevices symlinks survive across driver
     * reinstall within a single boot session — without this, the second
     * load returns STATUS_OBJECT_NAME_COLLISION (Code 37). */
    IoDeleteSymbolicLink(&g_SymbolicLink);  /* errors ignored — link may not exist */

    NTSTATUS cdStatus = IoCreateDevice(
        DriverObject,
        0,                          /* no device-extension; we keep state globally */
        &g_ControlDeviceName,
        FILE_DEVICE_UNKNOWN,
        FILE_DEVICE_SECURE_OPEN,    /* honour ACL set on the symlink */
        FALSE,                      /* not exclusive — multiple opens OK */
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
            cdStatus = IoCreateDevice(
                DriverObject, 0, &altName,
                FILE_DEVICE_UNKNOWN, FILE_DEVICE_SECURE_OPEN, FALSE,
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
