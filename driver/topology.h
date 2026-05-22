/*
 * topology.h - IMiniportTopology implementation declaration.
 *
 * The topology miniport's job is to expose the audio control surface:
 * volume, mute, and a DAC terminator. We do NOT apply the volume to
 * the PCM stream; the user-mode service forwards it to the Sonos
 * speaker. The driver simply stores the current value, fires KS
 * property change events, and posts STREAM_TO_SPEAKER_CONTROL_EVENT records
 * into the IOCTL queue.
 */

#pragma once

#include "driver.h"
#include "mintopo.h"
#include "ioctl.h"

class CMiniportTopology :
    public IMiniportTopology,
    public CUnknown
{
public:
    DECLARE_STD_UNKNOWN();
    DEFINE_STD_CONSTRUCTOR(CMiniportTopology);
    ~CMiniportTopology();

    /* IMiniport */
    STDMETHODIMP GetDescription(_Out_ PPCFILTER_DESCRIPTOR* OutFilterDescriptor) override;
    STDMETHODIMP DataRangeIntersection(
        _In_  ULONG               PinId,
        _In_  PKSDATARANGE        DataRange,
        _In_  PKSDATARANGE        MatchingDataRange,
        _In_  ULONG               OutputBufferLength,
        _Out_writes_bytes_to_opt_(OutputBufferLength, *ResultantFormatLength) PVOID ResultantFormat,
        _Out_ PULONG              ResultantFormatLength) override;

    /* IMiniportTopology */
    STDMETHODIMP Init(
        _In_ PUNKNOWN     UnknownAdapter,
        _In_ PRESOURCELIST ResourceList,
        _In_ PPORTTOPOLOGY Port) override;

    /* Property dispatch — invoked through KS. */
    NTSTATUS PropertyHandlerVolumeLevel(_In_ PPCPROPERTY_REQUEST Request);
    NTSTATUS PropertyHandlerMute       (_In_ PPCPROPERTY_REQUEST Request);
    NTSTATUS PropertyHandlerCpuResources(_In_ PPCPROPERTY_REQUEST Request);

    /* Called from IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME so the topology mirrors
     * an externally-driven change (e.g. user touched the physical
     * Sonos volume buttons). Fires a KS property change to make the
     * Windows mixer follow along. */
    VOID OnExternalVolumeChange(_In_ INT32 Mb, _In_ BOOLEAN Muted);

    /* Hook to the device extension so we can post control events. */
    VOID SetDeviceExtension(_In_ PSTREAM_TO_SPEAKER_DEVICE_EXTENSION Ext) {
        m_Ext = Ext;
    }

private:
    PPORTTOPOLOGY                 m_Port;
    PUNKNOWN                      m_UnknownAdapter;
    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION   m_Ext;
    /* IPortEvents lets us notify clients (Windows audio engine, mixer)
     * that a topology control changed; we query it from m_Port in Init. */
    PPORTEVENTS                   m_PortEvents;

    /* Per-channel volume (millibels). Two channels: L, R. */
    LONG                          m_VolumeMb[STREAM_TO_SPEAKER_CHANNELS];
    BOOLEAN                       m_Muted;
    KSPIN_LOCK                    m_ControlLock;

    VOID PostVolumeEvent_NoLock();
    VOID PostMuteEvent_NoLock();
    VOID FireControlChange(_In_ ULONG NodeId);
};
