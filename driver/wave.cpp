/*
 * wave.cpp - Wave filter descriptor, miniport implementation, and
 *            shared waveformat data.
 */

#include "wave.h"
#include "wavestream.h"

/* ------------------------------------------------------------------ */
/* WAVEFORMATEXTENSIBLE describing L16/44.1k/stereo                    */
/* ------------------------------------------------------------------ */

static const WAVEFORMATEXTENSIBLE g_WaveFormat =
{
    {
        WAVE_FORMAT_EXTENSIBLE,
        (WORD)STREAM_TO_SPEAKER_CHANNELS,
        (DWORD)STREAM_TO_SPEAKER_SAMPLE_RATE,
        (DWORD)(STREAM_TO_SPEAKER_SAMPLE_RATE * STREAM_TO_SPEAKER_FRAME_BYTES),
        (WORD)STREAM_TO_SPEAKER_FRAME_BYTES,
        (WORD)STREAM_TO_SPEAKER_BITS_PER_SAMPLE,
        sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)
    },
    { (WORD)STREAM_TO_SPEAKER_BITS_PER_SAMPLE },
    KSAUDIO_SPEAKER_STEREO,
    /* SubFormat = KSDATAFORMAT_SUBTYPE_PCM. */
    {
        0x00000001, 0x0000, 0x0010,
        { 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71 }
    }
};

const WAVEFORMATEXTENSIBLE*
StreamToSpeakerWaveFormat()
{
    return &g_WaveFormat;
}

/* ------------------------------------------------------------------ */
/* Data ranges for the wave pin                                        */
/* ------------------------------------------------------------------ */

static KSDATARANGE_AUDIO PinDataRangesPCM[] =
{
    {
        {
            sizeof(KSDATARANGE_AUDIO),
            0,
            0,
            0,
            STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
            STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM),
            STATICGUIDOF(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)
        },
        STREAM_TO_SPEAKER_CHANNELS,
        STREAM_TO_SPEAKER_BITS_PER_SAMPLE,
        STREAM_TO_SPEAKER_BITS_PER_SAMPLE,
        STREAM_TO_SPEAKER_SAMPLE_RATE,
        STREAM_TO_SPEAKER_SAMPLE_RATE
    }
};

static PKSDATARANGE PinDataRangePointersPCM[] =
{
    reinterpret_cast<PKSDATARANGE>(&PinDataRangesPCM[0])
};

/* Bridge pin (output) advertises a generic analog data range. */
static KSDATARANGE PinDataRangesBridge[] =
{
    {
        sizeof(KSDATARANGE),
        0, 0, 0,
        STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_ANALOG),
        STATICGUIDOF(KSDATAFORMAT_SPECIFIER_NONE)
    }
};

static PKSDATARANGE PinDataRangePointersBridge[] =
{
    &PinDataRangesBridge[0]
};

/* ------------------------------------------------------------------ */
/* PCPIN_DESCRIPTOR table                                              */
/* ------------------------------------------------------------------ */

static PCPIN_DESCRIPTOR WaveMiniportPins[] =
{
    /* PIN 0: render sink (data IN from engine).
     * Instance counts are pin INSTANCES, not channels — one
     * instance is plenty for a render endpoint. */
    {
        1, 1, 0,    /* MaxGlobal, MaxFilter, MinFilter instances */
        NULL,       /* AutomationTable                            */
        {
            0, NULL, 0, NULL,
            SIZEOF_ARRAY(PinDataRangePointersPCM),
            PinDataRangePointersPCM,
            KSPIN_DATAFLOW_IN,
            KSPIN_COMMUNICATION_SINK,
            &KSCATEGORY_AUDIO,
            NULL,
            0
        }
    },
    /* PIN 1: bridge (logical out to topology).
     * Bridge pins still need MaxFilter>=1, otherwise PortCls
     * can't materialise a connection target.
     * Category is KSCATEGORY_AUDIO (sysvad pattern) — Category
     * expects a KSCATEGORY_* GUID, not a KSNODETYPE_*. */
    {
        1, 1, 0,
        NULL,
        {
            0, NULL, 0, NULL,
            SIZEOF_ARRAY(PinDataRangePointersBridge),
            PinDataRangePointersBridge,
            KSPIN_DATAFLOW_OUT,
            KSPIN_COMMUNICATION_NONE,
            &KSCATEGORY_AUDIO,
            NULL,
            0
        }
    }
};

static PCCONNECTION_DESCRIPTOR WaveMiniportConnections[] =
{
    { PCFILTER_NODE, KSPIN_WAVE_RENDER_SINK,   PCFILTER_NODE, KSPIN_WAVE_RENDER_SOURCE }
};

static const GUID WaveMiniportCategories[] =
{
    STATICGUIDOF(KSCATEGORY_AUDIO),
    STATICGUIDOF(KSCATEGORY_RENDER),
    STATICGUIDOF(KSCATEGORY_REALTIME)
};

static PCFILTER_DESCRIPTOR WaveMiniportFilterDescriptor =
{
    0,                                              /* Version          */
    NULL,                                           /* AutomationTable  */
    sizeof(PCPIN_DESCRIPTOR),                       /* PinSize          */
    SIZEOF_ARRAY(WaveMiniportPins),                 /* PinCount         */
    WaveMiniportPins,                               /* Pins             */
    0,                                              /* NodeSize         */
    0,                                              /* NodeCount        */
    NULL,                                           /* Nodes            */
    SIZEOF_ARRAY(WaveMiniportConnections),          /* ConnectionCount  */
    WaveMiniportConnections,                        /* Connections      */
    SIZEOF_ARRAY(WaveMiniportCategories),           /* CategoryCount    */
    WaveMiniportCategories                          /* Categories       */
};

const PCFILTER_DESCRIPTOR*
StreamToSpeakerWaveFilterDescriptor()
{
    return &WaveMiniportFilterDescriptor;
}

/* ------------------------------------------------------------------ */
/* CMiniportWaveRT                                                     */
/* ------------------------------------------------------------------ */

NTSTATUS
CreateMiniportWaveRTStreamToSpeaker(
    _Out_ PUNKNOWN*  Unknown,
    _In_  REFCLSID   RefClsId,
    _In_opt_ PUNKNOWN OuterUnknown,
    _In_  POOL_FLAGS PoolFlags)
{
    UNREFERENCED_PARAMETER(RefClsId);
    PAGED_CODE();
    if (Unknown == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    CMiniportWaveRT* p = new (PoolFlags, STREAM_TO_SPEAKER_POOL_TAG)
        CMiniportWaveRT(OuterUnknown);
    if (p == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    *Unknown = PUNKNOWN((PMINIPORTWAVERT)p);
    (*Unknown)->AddRef();
    return STATUS_SUCCESS;
}

CMiniportWaveRT::~CMiniportWaveRT()
{
    if (m_Port != nullptr) {
        m_Port->Release();
        m_Port = nullptr;
    }
}

STDMETHODIMP
CMiniportWaveRT::NonDelegatingQueryInterface(
    _In_ REFIID Interface,
    _COM_Outptr_ PVOID* Object)
{
    PAGED_CODE();
    if (Object == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    if (IsEqualGUIDAligned(Interface, IID_IUnknown)) {
        *Object = PVOID(PUNKNOWN(PMINIPORTWAVERT(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniport) ||
               IsEqualGUIDAligned(Interface, IID_IMiniportWaveRT)) {
        *Object = PVOID(PMINIPORTWAVERT(this));
    } else {
        *Object = nullptr;
    }
    if (*Object != nullptr) {
        PUNKNOWN(*Object)->AddRef();
        return STATUS_SUCCESS;
    }
    return STATUS_INVALID_PARAMETER;
}

STDMETHODIMP
CMiniportWaveRT::GetDescription(_Out_ PPCFILTER_DESCRIPTOR* OutFilterDescriptor)
{
    PAGED_CODE();
    if (OutFilterDescriptor == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    *OutFilterDescriptor = &WaveMiniportFilterDescriptor;
    return STATUS_SUCCESS;
}

STDMETHODIMP
CMiniportWaveRT::DataRangeIntersection(
    _In_ ULONG               PinId,
    _In_ PKSDATARANGE        ClientDataRange,
    _In_ PKSDATARANGE        MyDataRange,
    _In_ ULONG               OutputBufferLength,
    _Out_writes_bytes_to_opt_(OutputBufferLength, *ResultantFormatLength) PVOID ResultantFormat,
    _Out_ PULONG             ResultantFormatLength)
{
    PAGED_CODE();
    UNREFERENCED_PARAMETER(MyDataRange);

    if (ResultantFormatLength == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    *ResultantFormatLength = 0;

    if (PinId != KSPIN_WAVE_RENDER_SINK) {
        return STATUS_NOT_SUPPORTED;
    }

    /* Only KSDATAFORMAT_TYPE_AUDIO + PCM. */
    if (!IsEqualGUIDAligned(ClientDataRange->MajorFormat, KSDATAFORMAT_TYPE_AUDIO) ||
        !IsEqualGUIDAligned(ClientDataRange->SubFormat,   KSDATAFORMAT_SUBTYPE_PCM)) {
        return STATUS_NO_MATCH;
    }

    ULONG needed = sizeof(KSDATAFORMAT_WAVEFORMATEX) + sizeof(WAVEFORMATEXTENSIBLE) -
                   sizeof(WAVEFORMATEX);
    if (OutputBufferLength == 0) {
        *ResultantFormatLength = needed;
        return STATUS_BUFFER_OVERFLOW;
    }
    if (OutputBufferLength < needed) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    if (ResultantFormat == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }

    PKSDATAFORMAT_WAVEFORMATEX out =
        static_cast<PKSDATAFORMAT_WAVEFORMATEX>(ResultantFormat);
    RtlZeroMemory(out, needed);
    out->DataFormat.FormatSize  = needed;
    out->DataFormat.Flags       = 0;
    out->DataFormat.SampleSize  = STREAM_TO_SPEAKER_FRAME_BYTES;
    out->DataFormat.Reserved    = 0;
    out->DataFormat.MajorFormat = KSDATAFORMAT_TYPE_AUDIO;
    out->DataFormat.SubFormat   = KSDATAFORMAT_SUBTYPE_PCM;
    out->DataFormat.Specifier   = KSDATAFORMAT_SPECIFIER_WAVEFORMATEX;

    PWAVEFORMATEXTENSIBLE wfx =
        reinterpret_cast<PWAVEFORMATEXTENSIBLE>(&out->WaveFormatEx);
    RtlCopyMemory(wfx, &g_WaveFormat, sizeof(WAVEFORMATEXTENSIBLE));

    *ResultantFormatLength = needed;
    return STATUS_SUCCESS;
}

STDMETHODIMP
CMiniportWaveRT::Init(
    _In_ PUNKNOWN     UnknownAdapter,
    _In_ PRESOURCELIST ResourceList,
    _In_ PPORTWAVERT  Port)
{
    UNREFERENCED_PARAMETER(ResourceList);
    PAGED_CODE();
    /* UnknownAdapter may be nullptr — virtual driver with no adapter
     * common object. Only Port is required. */
    if (Port == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    m_Port = Port;
    m_Port->AddRef();
    m_UnknownAdapter = UnknownAdapter;  /* may be nullptr */
    return STATUS_SUCCESS;
}

STDMETHODIMP
CMiniportWaveRT::NewStream(
    _Out_ PMINIPORTWAVERTSTREAM* OutStream,
    _In_  PPORTWAVERTSTREAM      PortStream,
    _In_  ULONG                  Pin,
    _In_  BOOLEAN                Capture,
    _In_  PKSDATAFORMAT          DataFormat)
{
    PAGED_CODE();
    if (OutStream == nullptr || DataFormat == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    if (Capture) {
        return STATUS_NOT_SUPPORTED;
    }
    if (Pin != KSPIN_WAVE_RENDER_SINK) {
        return STATUS_INVALID_PARAMETER;
    }
    *OutStream = nullptr;

    CMiniportWaveRTStream* stream =
        new (POOL_FLAG_NON_PAGED, STREAM_TO_SPEAKER_POOL_TAG)
            CMiniportWaveRTStream(nullptr);
    if (stream == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    NTSTATUS status = stream->Init(this, PortStream, Pin, DataFormat);
    if (!NT_SUCCESS(status)) {
        delete stream;
        return status;
    }
    *OutStream = PMINIPORTWAVERTSTREAM(stream);
    PUNKNOWN(PMINIPORTWAVERTSTREAM(stream))->AddRef();

    if (m_Ext != nullptr) {
        m_Ext->ActiveStream = stream;
    }
    return STATUS_SUCCESS;
}

STDMETHODIMP
CMiniportWaveRT::GetDeviceDescription(_Out_ PDEVICE_DESCRIPTION DeviceDescription)
{
    PAGED_CODE();
    if (DeviceDescription == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlZeroMemory(DeviceDescription, sizeof(*DeviceDescription));
    DeviceDescription->Master           = TRUE;
    DeviceDescription->ScatterGather    = TRUE;
    DeviceDescription->Dma32BitAddresses= TRUE;
    DeviceDescription->Dma64BitAddresses= TRUE;
    DeviceDescription->InterfaceType    = PNPBus;
    DeviceDescription->MaximumLength    = 0x10000;
    return STATUS_SUCCESS;
}
