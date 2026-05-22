/*
 * adapter.cpp - PortCls adapter glue (AddDevice / StartDevice).
 *
 * The driver is a single virtual device. AddDevice creates a PortCls
 * FDO, allocates the STREAM_TO_SPEAKER_DEVICE_EXTENSION, and StartDevice
 * registers two subdevices (wave + topology) and wires them with a
 * PhysicalConnection. The same FDO also handles the user-mode IOCTLs
 * (see driver.cpp).
 */

#include "driver.h"
#include "ioctl.h"
#include "wave.h"
#include "topology.h"

/* GUIDs PortCls uses internally to identify the two subdevices. We
 * pick stable names — they're not exposed to user-mode. */
static const WCHAR* const kWaveSubdeviceName = L"Wave";
static const WCHAR* const kTopoSubdeviceName = L"Topology";

static NTSTATUS
InstallSubdevice(
    _In_  PDEVICE_OBJECT  DeviceObject,
    _In_  PIRP            Irp,
    _In_  PCWSTR          Name,
    _In_  REFGUID         PortClassId,
    _In_  REFGUID         MiniportClassId,
    _In_  PMINIPORT       Miniport,
    _In_  PUNKNOWN        UnknownAdapter,
    _Out_opt_ PUNKNOWN*   OutMiniport,
    _Out_opt_ PUNKNOWN*   OutPort)
{
    PAGED_CODE();
    DBG_INFO("InstallSubdevice '%ws' begin", Name);

    PPORT port = nullptr;
    NTSTATUS status = PcNewPort(&port, PortClassId);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("PcNewPort '%ws' failed 0x%08x", Name, status);
        return status;
    }
    DBG_INFO("PcNewPort '%ws' ok", Name);

    status = port->Init(DeviceObject, Irp, Miniport, UnknownAdapter, nullptr);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("port->Init '%ws' failed 0x%08x", Name, status);
        port->Release();
        return status;
    }
    DBG_INFO("port->Init '%ws' ok", Name);

    /* PcRegisterSubdevice wants a plain wide string name, not a
     * UNICODE_STRING. Cast away const because the prototype uses PWCHAR. */
    status = PcRegisterSubdevice(DeviceObject,
                                 const_cast<PWCHAR>(Name),
                                 port);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("PcRegisterSubdevice '%ws' failed 0x%08x", Name, status);
        port->Release();
        return status;
    }
    DBG_INFO("PcRegisterSubdevice '%ws' ok", Name);

    if (OutPort != nullptr) {
        *OutPort = PUNKNOWN(port);
        PUNKNOWN(port)->AddRef();
    }
    if (OutMiniport != nullptr) {
        *OutMiniport = PUNKNOWN(Miniport);
        PUNKNOWN(Miniport)->AddRef();
    }

    port->Release();
    UNREFERENCED_PARAMETER(MiniportClassId);
    return STATUS_SUCCESS;
}

static NTSTATUS
ConnectSubdevices(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PUNKNOWN       FromUnknown,
    _In_ ULONG          FromPin,
    _In_ PUNKNOWN       ToUnknown,
    _In_ ULONG          ToPin)
{
    PAGED_CODE();
    /* Modern PortCls: PcRegisterPhysicalConnection takes IUnknown
     * pointers to the two miniports (or their ports) — not names. */
    return PcRegisterPhysicalConnection(DeviceObject,
                                        FromUnknown, FromPin,
                                        ToUnknown,   ToPin);
}

extern "C" NTSTATUS NTAPI
StreamToSpeakerStartDevice(
    _In_ PDEVICE_OBJECT  DeviceObject,
    _In_ PIRP            Irp,
    _In_ PRESOURCELIST   ResourceList)
{
    PAGED_CODE();
    UNREFERENCED_PARAMETER(ResourceList);
    DBG_INFO("StreamToSpeakerStartDevice entry DO=%p", DeviceObject);

    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION ext =
        StreamToSpeakerGetExt(DeviceObject);
    if (ext == nullptr) {
        DBG_ERROR("StartDevice: ext is null");
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }

    /* Construct wave miniport. */
    PUNKNOWN waveUnknown = nullptr;
    NTSTATUS status = CreateMiniportWaveRTStreamToSpeaker(
        &waveUnknown, CLSID_NULL, nullptr, POOL_FLAG_NON_PAGED);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("CreateMiniportWaveRTStreamToSpeaker failed 0x%08x", status);
        return status;
    }
    DBG_INFO("CreateMiniportWaveRTStreamToSpeaker ok");

    PMINIPORTWAVERT waveMiniport = nullptr;
    status = waveUnknown->QueryInterface(IID_IMiniportWaveRT,
                                         reinterpret_cast<PVOID*>(&waveMiniport));
    waveUnknown->Release();
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("QueryInterface(IMiniportWaveRT) failed 0x%08x", status);
        return status;
    }
    DBG_INFO("QueryInterface(IMiniportWaveRT) ok");

    static_cast<CMiniportWaveRT*>(waveMiniport)->SetDeviceExtension(ext);

    /* Construct topology miniport. */
    PUNKNOWN topoUnknown = nullptr;
    status = CreateMiniportTopologyStreamToSpeaker(
        &topoUnknown, CLSID_NULL, nullptr, POOL_FLAG_NON_PAGED);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("CreateMiniportTopologyStreamToSpeaker failed 0x%08x", status);
        waveMiniport->Release();
        return status;
    }
    DBG_INFO("CreateMiniportTopologyStreamToSpeaker ok");

    PMINIPORTTOPOLOGY topoMiniport = nullptr;
    status = topoUnknown->QueryInterface(IID_IMiniportTopology,
                                         reinterpret_cast<PVOID*>(&topoMiniport));
    topoUnknown->Release();
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("QueryInterface(IMiniportTopology) failed 0x%08x", status);
        waveMiniport->Release();
        return status;
    }
    DBG_INFO("QueryInterface(IMiniportTopology) ok");

    static_cast<CMiniportTopology*>(topoMiniport)->SetDeviceExtension(ext);
    ext->Topology = static_cast<CMiniportTopology*>(topoMiniport);

    /* Register subdevices. Capture each port's IUnknown so we can pass
     * them to PcRegisterPhysicalConnection below. */
    PUNKNOWN wavePortUnknown = nullptr;
    PUNKNOWN topoPortUnknown = nullptr;

    status = InstallSubdevice(
        DeviceObject, Irp, kWaveSubdeviceName,
        CLSID_PortWaveRT, CLSID_NULL,
        PMINIPORT(waveMiniport),
        nullptr,
        nullptr, &wavePortUnknown);
    if (!NT_SUCCESS(status)) {
        topoMiniport->Release();
        waveMiniport->Release();
        return status;
    }

    status = InstallSubdevice(
        DeviceObject, Irp, kTopoSubdeviceName,
        CLSID_PortTopology, CLSID_NULL,
        PMINIPORT(topoMiniport),
        nullptr,
        nullptr, &topoPortUnknown);
    if (!NT_SUCCESS(status)) {
        if (wavePortUnknown != nullptr) wavePortUnknown->Release();
        topoMiniport->Release();
        waveMiniport->Release();
        return status;
    }

    /* Connect Wave PIN_BRIDGE -> Topo PIN_WAVEOUT using IUnknown
     * pointers — modern PortCls signature. */
    DBG_INFO("ConnectSubdevices wave[%u] -> topo[%u]",
             (ULONG)KSPIN_WAVE_RENDER_SOURCE, (ULONG)KSPIN_TOPOLOGY_WAVEOUT_SOURCE);
    status = ConnectSubdevices(
        DeviceObject,
        wavePortUnknown, KSPIN_WAVE_RENDER_SOURCE,
        topoPortUnknown, KSPIN_TOPOLOGY_WAVEOUT_SOURCE);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("PcRegisterPhysicalConnection failed 0x%08x", status);
    } else {
        DBG_INFO("StartDevice complete");
    }

    if (wavePortUnknown != nullptr) wavePortUnknown->Release();
    if (topoPortUnknown != nullptr) topoPortUnknown->Release();
    topoMiniport->Release();
    waveMiniport->Release();

    return status;
}

extern "C" NTSTATUS
StreamToSpeakerAddDevice(
    _In_ PDRIVER_OBJECT  DriverObject,
    _In_ PDEVICE_OBJECT  PhysicalDeviceObject)
{
    PAGED_CODE();

    /* PortCls reserves PORT_CLASS_DEVICE_EXTENSION_SIZE (512 on x64)
     * bytes of the FDO DeviceExtension for itself. The DeviceExtensionSize
     * we pass MUST be 0 *or* >= PORT_CLASS_DEVICE_EXTENSION_SIZE — any
     * value strictly between is rejected with STATUS_INVALID_PARAMETER.
     * We pass header + our struct so our data lives at offset 512. */
    const ULONG totalExtSize =
        PORT_CLASS_DEVICE_EXTENSION_SIZE + sizeof(STREAM_TO_SPEAKER_DEVICE_EXTENSION);

    DBG_INFO("StreamToSpeakerAddDevice entry, DO=%p PDO=%p MaxMP=%u ExtSize=%u (port=%u+ours=%u) StartDev=%p",
             DriverObject, PhysicalDeviceObject,
             STREAM_TO_SPEAKER_MAX_MINIPORTS, totalExtSize,
             (ULONG)PORT_CLASS_DEVICE_EXTENSION_SIZE,
             (ULONG)sizeof(STREAM_TO_SPEAKER_DEVICE_EXTENSION),
             (PVOID)StreamToSpeakerStartDevice);

    NTSTATUS status = PcAddAdapterDevice(
        DriverObject,
        PhysicalDeviceObject,
        PCPFNSTARTDEVICE(StreamToSpeakerStartDevice),
        STREAM_TO_SPEAKER_MAX_MINIPORTS,
        totalExtSize);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("PcAddAdapterDevice failed 0x%08x", status);
        return status;
    }
    DBG_INFO("PcAddAdapterDevice succeeded");

    status = StreamToSpeakerAddDeviceInit(DriverObject, PhysicalDeviceObject);
    if (!NT_SUCCESS(status)) {
        DBG_ERROR("StreamToSpeakerAddDeviceInit failed 0x%08x", status);
        return status;
    }
    return STATUS_SUCCESS;
}
