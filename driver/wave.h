/*
 * wave.h - IMiniportWaveRT implementation declarations.
 *
 * One render pin, fixed format L16/44.1k/stereo. The class is small;
 * it allocates a CMiniportWaveRTStream on NewStream() and otherwise
 * trampolines IMiniport calls.
 */

#pragma once

#include "driver.h"
#include "minwave.h"

class CMiniportWaveRTStream;

class CMiniportWaveRT :
    public IMiniportWaveRT,
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
