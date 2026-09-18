/*
 * wave.h - IMiniportWaveRT implementation declarations.
 *
 * One render pin, L16/44.1k, mono or stereo (the ring buffer the
 * service reads is always stereo; mono streams are up-mixed in the
 * consumer DPC). The class is small; it allocates a
 * CMiniportWaveRTStream on NewStream() and otherwise trampolines
 * IMiniport calls.
 *
 * IMiniportAudioSignalProcessing is implemented so that PortCls can
 * answer KSPROPERTY_AUDIOSIGNALPROCESSING_MODES on the render pin —
 * a required pin property since Windows 10 (HLK "KS Topology Test",
 * TC_CheckProcessingModes).
 */

#pragma once

#include "driver.h"
#include "minwave.h"

class CMiniportWaveRTStream;

class CMiniportWaveRT :
    public IMiniportWaveRT,
    public IMiniportAudioSignalProcessing,
    public CUnknown
{
public:
    DECLARE_STD_UNKNOWN();
    DEFINE_STD_CONSTRUCTOR(CMiniportWaveRT);
    ~CMiniportWaveRT();

    /* IMiniport */
    STDMETHODIMP GetDescription(_Out_ PPCFILTER_DESCRIPTOR* OutFilterDescriptor) override;
    STDMETHODIMP DataRangeIntersection(
        _In_ ULONG               PinId,
        _In_ PKSDATARANGE        ClientDataRange,
        _In_ PKSDATARANGE        MyDataRange,
        _In_ ULONG               OutputBufferLength,
        _Out_writes_bytes_to_opt_(OutputBufferLength, *ResultantFormatLength) PVOID ResultantFormat,
        _Out_ PULONG             ResultantFormatLength) override;

    /* IMiniportWaveRT */
    STDMETHODIMP Init(
        _In_ PUNKNOWN     UnknownAdapter,
        _In_ PRESOURCELIST ResourceList,
        _In_ PPORTWAVERT  Port) override;
    STDMETHODIMP NewStream(
        _Out_ PMINIPORTWAVERTSTREAM* OutStream,
        _In_  PPORTWAVERTSTREAM      PortStream,
        _In_  ULONG                  Pin,
        _In_  BOOLEAN                Capture,
        _In_  PKSDATAFORMAT          DataFormat) override;
    STDMETHODIMP GetDeviceDescription(_Out_ PDEVICE_DESCRIPTION DeviceDescription) override;

    /* IMiniportAudioSignalProcessing */
    STDMETHODIMP_(NTSTATUS) GetModes(
        _In_                                        ULONG  Pin,
        _Out_writes_opt_(*NumSignalProcessingModes) GUID*  SignalProcessingModes,
        _Inout_                                     ULONG* NumSignalProcessingModes) override;

    /* Hook for the device extension to find the active stream. */
    VOID SetDeviceExtension(_In_ PSTREAM_TO_SPEAKER_DEVICE_EXTENSION Ext) {
        m_Ext = Ext;
    }
    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION DeviceExtension() const { return m_Ext; }

private:
    PPORTWAVERT                m_Port;
    PUNKNOWN                   m_UnknownAdapter;
    PSTREAM_TO_SPEAKER_DEVICE_EXTENSION m_Ext;
};

/* Channel count (1 or 2) if the KSDATAFORMAT describes a PCM format
 * this driver can stream (L16 @ 44.1 kHz), else 0. Shared by the
 * proposed-format handler, DataRangeIntersection and NewStream. */
ULONG StreamToSpeakerSupportedChannels(_In_reads_bytes_(Size) const KSDATAFORMAT* Format, _In_ ULONG Size);
