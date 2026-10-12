/*
 * driver.h - Common declarations and forward decls for the StreamToSpeaker
 *            virtual audio driver.
 *
 * Pulls in the WDK audio headers and the shared user-mode ABI
 * (stream_to_speaker_ioctl.h). Defines the per-device extension structure and
 * forward-declares all the major helpers exported between translation
 * units.
 */

#pragma once

#ifdef EMULATE_WINDOWS_KERNEL
  /* Standalone syntax-check path: pull in a tiny shim of the kernel
   * types so the file parses without the WDK. Make stream_to_speaker_ioctl.h
   * take its kernel-mode branch. */
  #ifndef _KERNEL_MODE
    #define _KERNEL_MODE 1
  #endif
  #include "emul_wdk.h"
  /* Provide ntddk.h's IOCTL helpers via the shim. */
  #include "../include/stream_to_speaker_ioctl.h"
#else
  /* _KERNEL_MODE and _NTDDK_ are defined by the WindowsKernelModeDriver
     toolset automatically; do not redefine here. */

  /* ntifs.h first: it includes ntddk.h itself and must precede it (the
   * CREATE handler queries the caller's token integrity level). */
  #include <ntifs.h>
  #include <ntddk.h>
  #include <wdm.h>
  #include <windef.h>
  #include <portcls.h>
  #include <ksdebug.h>
  #include <ks.h>
  #include <ksmedia.h>
  #include <stdunk.h>

  #include "../include/stream_to_speaker_ioctl.h"
#endif

#include "debug.h"

/* ---------------------------------------------------------------------
 * Compile-time configuration
 * ------------------------------------------------------------------- */

/* Build number reported via IOCTL_STREAM_TO_SPEAKER_GET_VERSION. Bump when
 * shipping a new driver binary. Inspect the service-side log line
 *   "StreamToSpeaker driver opened (proto=1 build=N ...)"
 * after install to confirm the kernel actually loaded the new bits —
 * pnputil /add-driver only stages the driver; you still need to bounce
 * the device (Device Manager → disable / enable, or a reboot) for the
 * new binary to be in-memory. */
#define STREAM_TO_SPEAKER_DRIVER_BUILD          8u

/* Stream format constants. v1 supports one fixed format. */
#define STREAM_TO_SPEAKER_SAMPLE_RATE           44100u
#define STREAM_TO_SPEAKER_BITS_PER_SAMPLE       16u
#define STREAM_TO_SPEAKER_CHANNELS              2u
#define STREAM_TO_SPEAKER_FRAME_BYTES           ((STREAM_TO_SPEAKER_BITS_PER_SAMPLE / 8u) * STREAM_TO_SPEAKER_CHANNELS)

/* WaveRT cyclic buffer geometry. Notification interval 2 ms, two
 * periods => 4 ms of cyclic buffer. */
#define STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS  2u
#define STREAM_TO_SPEAKER_WAVERT_PERIODS            2u

/* Ring buffer between WaveRT DPC and IOCTL completion path. 32 KB at
 * 44.1 kHz/16/stereo is ~186 ms of audio. */
#define STREAM_TO_SPEAKER_RING_BYTES                (32u * 1024u)

/* Topology defaults. */
#define STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS      (-9600)
#define STREAM_TO_SPEAKER_VOLUME_MAX_MILLIBELS      (0)
#define STREAM_TO_SPEAKER_VOLUME_STEP_MILLIBELS     (50)
#define STREAM_TO_SPEAKER_VOLUME_DEFAULT_MILLIBELS  (0)

/* Convenience pool tag. */
#define STREAM_TO_SPEAKER_POOL_TAG                  'fmyS'

/* Reference string appended to the device-interface registration.
 * User-mode CreateFileW on the resulting path puts this in
 * FileObject->FileName; our CREATE dispatcher uses that to distinguish
 * control opens from KS audio opens (SAR-style discriminator). The
 * specific value doesn't matter, only that it's stable and recognised
 * on both sides. KS rejects opens with empty FileName, so any non-empty
 * literal works as long as it doesn't collide with a KS pin name. */
#define STREAM_TO_SPEAKER_CONTROL_REF_STRING        L"control"

/* Max subdevices passed to PcAddAdapterDevice. 2 is enough for our
 * wave + topology miniports but some WDK builds enforce a higher
 * minimum; sysvad uses 3, we round to 8 for headroom. */
#define STREAM_TO_SPEAKER_MAX_MINIPORTS             8u

/* ---------------------------------------------------------------------
 * Placement new / delete operator declarations.
 *
 * WDK 10.0.22000+ removed the inline operator new/delete from stdunk.h.
 * Definitions live in newdelete.cpp. We provide the modern POOL_FLAGS-
 * based overloads so `new (POOL_FLAG_NON_PAGED, TAG) Class(...)` works.
 * ------------------------------------------------------------------- */

#ifndef EMULATE_WINDOWS_KERNEL
/* Only the POOL_FLAGS overloads — operator delete forms come from
 * stdunk.h's inline versions and don't need redeclaring. */
PVOID operator new(size_t iSize, POOL_FLAGS poolFlags, ULONG tag);
PVOID operator new(size_t iSize, POOL_FLAGS poolFlags);
#endif

/* ---------------------------------------------------------------------
 * Forward declarations
 * ------------------------------------------------------------------- */

struct StreamToSpeakerRingBuffer;
struct StreamToSpeakerIoctlCtx;
class  CMiniportWaveRT;
class  CMiniportWaveRTStream;
class  CMiniportTopology;

/* The per-device extension we hang off the PortCls FDO. */
typedef struct _STREAM_TO_SPEAKER_DEVICE_EXTENSION {
    PDEVICE_OBJECT          DeviceObject;
    PDEVICE_OBJECT          PhysicalDeviceObject;
    PDEVICE_OBJECT          NextDeviceObject;

    PDRIVER_DISPATCH        PortClsDispatch[IRP_MJ_MAXIMUM_FUNCTION + 1];

    UNICODE_STRING          DeviceInterfaceLink;
    BOOLEAN                 DeviceInterfaceRegistered;
    BOOLEAN                 SymbolicLinkCreated;

    StreamToSpeakerIoctlCtx*      IoctlCtx;
    /* Entry in the driver's list of devices whose IoctlCtx can be
     * published to the control device (guarded by a driver-wide mutex). */
    LIST_ENTRY              LiveLink;
    /* TRUE once IoctlCtx has been withdrawn from the control device
     * (surprise removal or remove): no IOCTL can reach it any more and
     * every IRP that was queued on it has been completed. */
    BOOLEAN                 IoctlCtxWithdrawn;

    /* Topology miniport that IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME forwards
     * to. Published with its own reference only after StartDevice has
     * fully succeeded, and withdrawn (waiting for in-flight users) before
     * PortCls processes STOP / SURPRISE_REMOVAL / REMOVE. Never read it
     * directly from the IOCTL path: use TopologyForwardExternalVolume. */
    CMiniportTopology*      Topology;
    EX_RUNDOWN_REF          TopologyRundown;

    CMiniportWaveRTStream*  ActiveStream;
} STREAM_TO_SPEAKER_DEVICE_EXTENSION, *PSTREAM_TO_SPEAKER_DEVICE_EXTENSION;

/* ---------------------------------------------------------------------
 * Exports across translation units
 * ------------------------------------------------------------------- */

extern "C" NTSTATUS StreamToSpeakerAddDevice(
    _In_ PDRIVER_OBJECT  DriverObject,
    _In_ PDEVICE_OBJECT  PhysicalDeviceObject);

extern "C" NTSTATUS StreamToSpeakerAddDeviceInit(
    _In_ PDRIVER_OBJECT  DriverObject,
    _In_ PDEVICE_OBJECT  PhysicalDeviceObject);

/* PortCls reserves the first PORT_CLASS_DEVICE_EXTENSION_SIZE bytes
 * (= 64 * sizeof(ULONG_PTR) = 512 on x64) of the FDO's DeviceExtension
 * for itself. Our data lives immediately after. Any access to our
 * fields must go through this helper, NOT raw DeviceExtension. */
inline PSTREAM_TO_SPEAKER_DEVICE_EXTENSION
StreamToSpeakerGetExt(_In_ PDEVICE_OBJECT DeviceObject)
{
    if (DeviceObject == nullptr || DeviceObject->DeviceExtension == nullptr) {
        return nullptr;
    }
    return reinterpret_cast<PSTREAM_TO_SPEAKER_DEVICE_EXTENSION>(
        static_cast<PUCHAR>(DeviceObject->DeviceExtension)
        + PORT_CLASS_DEVICE_EXTENSION_SIZE);
}

/* Single read of the extension's IOCTL context for the audio-side
 * producers (WaveRT DPC, stream state changes, topology setters), so a
 * caller never checks one value and uses another. */
inline StreamToSpeakerIoctlCtx*
StreamToSpeakerExtIoctlCtx(_In_ PSTREAM_TO_SPEAKER_DEVICE_EXTENSION Ext)
{
    if (Ext == nullptr) {
        return nullptr;
    }
    return *reinterpret_cast<StreamToSpeakerIoctlCtx* volatile*>(&Ext->IoctlCtx);
}

/* Must be extern "C" + NTAPI so the function pointer matches the
 * PCPFNSTARTDEVICE typedef PortCls expects. Without these PortCls
 * can reject PcAddAdapterDevice with STATUS_INVALID_PARAMETER. */
extern "C" NTSTATUS NTAPI StreamToSpeakerStartDevice(
    _In_ PDEVICE_OBJECT     DeviceObject,
    _In_ PIRP               Irp,
    _In_ PRESOURCELIST      ResourceList);

#ifndef EMULATE_WINDOWS_KERNEL
extern "C" DRIVER_INITIALIZE DriverEntry;
#endif

extern "C" NTSTATUS StreamToSpeakerDispatchPnp(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" NTSTATUS StreamToSpeakerDispatchDeviceControl(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" NTSTATUS StreamToSpeakerDispatchCreateClose(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" NTSTATUS StreamToSpeakerDispatchCleanup(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" NTSTATUS StreamToSpeakerDispatchOther(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" NTSTATUS StreamToSpeakerDispatchPassThrough(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" NTSTATUS StreamToSpeakerDispatchPower(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" NTSTATUS StreamToSpeakerDispatchSystemControl(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP           Irp);

extern "C" VOID StreamToSpeakerDriverUnload(
    _In_ PDRIVER_OBJECT DriverObject);

/* Topology publication (topology.cpp). Publish/Withdraw run at
 * PASSIVE_LEVEL; Withdraw blocks until no IOCTL is using the miniport.
 * Forward is a no-op when no topology is published. */
VOID TopologyPublish(
    _In_ PSTREAM_TO_SPEAKER_DEVICE_EXTENSION Ext,
    _In_ CMiniportTopology*                  Topology);
VOID TopologyWithdraw(
    _In_ PSTREAM_TO_SPEAKER_DEVICE_EXTENSION Ext);
VOID TopologyForwardExternalVolume(
    _In_ PSTREAM_TO_SPEAKER_DEVICE_EXTENSION Ext,
    _In_ INT32                               LevelMillibels,
    _In_ BOOLEAN                             Muted);

NTSTATUS CreateMiniportWaveRTStreamToSpeaker(
    _Out_  PUNKNOWN*  Unknown,
    _In_   REFCLSID   RefClsId,
    _In_opt_ PUNKNOWN OuterUnknown,
    _In_   POOL_FLAGS PoolFlags);

NTSTATUS CreateMiniportTopologyStreamToSpeaker(
    _Out_  PUNKNOWN*  Unknown,
    _In_   REFCLSID   RefClsId,
    _In_opt_ PUNKNOWN OuterUnknown,
    _In_   POOL_FLAGS PoolFlags);

/* End of declarations. */
