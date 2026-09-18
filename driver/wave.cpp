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

/* Signal-processing modes the render pin supports. DEFAULT is what
 * the shared-mode engine uses; RAW lets exclusive-mode clients bypass
 * APO processing — for a passthrough virtual device both are simply
 * "the bits you give us". */
static const GUID g_SignalProcessingModes[] =
{
    STATIC_AUDIO_SIGNALPROCESSINGMODE_DEFAULT,
    STATIC_AUDIO_SIGNALPROCESSINGMODE_RAW
};

/* Fill a KSDATAFORMAT_WAVEFORMATEXTENSIBLE for `channels` (1 or 2)
 * channels of L16 @ 44.1 kHz. `needed` bytes are written. */
static const ULONG g_FormatBytes =
    sizeof(KSDATAFORMAT) + sizeof(WAVEFORMATEXTENSIBLE);

static VOID FillFormat(_Out_writes_bytes_(g_FormatBytes) PVOID Out, _In_ ULONG Channels)
{
    PKSDATAFORMAT_WAVEFORMATEXTENSIBLE fmt =
        static_cast<PKSDATAFORMAT_WAVEFORMATEXTENSIBLE>(Out);
    RtlZeroMemory(fmt, g_FormatBytes);
    RtlCopyMemory(&fmt->WaveFormatExt, &g_WaveFormat, sizeof(WAVEFORMATEXTENSIBLE));
    if (Channels == 1) {
        fmt->WaveFormatExt.Format.nChannels       = 1;
        fmt->WaveFormatExt.Format.nBlockAlign     = (WORD)(STREAM_TO_SPEAKER_BITS_PER_SAMPLE / 8u);
        fmt->WaveFormatExt.Format.nAvgBytesPerSec = STREAM_TO_SPEAKER_SAMPLE_RATE *
                                                    (STREAM_TO_SPEAKER_BITS_PER_SAMPLE / 8u);
        fmt->WaveFormatExt.dwChannelMask          = KSAUDIO_SPEAKER_MONO;
    }
    fmt->DataFormat.FormatSize  = g_FormatBytes;
    fmt->DataFormat.Flags       = 0;
    fmt->DataFormat.SampleSize  = fmt->WaveFormatExt.Format.nBlockAlign;
    fmt->DataFormat.Reserved    = 0;
    fmt->DataFormat.MajorFormat = KSDATAFORMAT_TYPE_AUDIO;
    fmt->DataFormat.SubFormat   = KSDATAFORMAT_SUBTYPE_PCM;
    fmt->DataFormat.Specifier   = KSDATAFORMAT_SPECIFIER_WAVEFORMATEX;
}

ULONG
StreamToSpeakerSupportedChannels(_In_reads_bytes_(Size) const KSDATAFORMAT* Format, _In_ ULONG Size)
{
    if (Format == nullptr || Size < sizeof(KSDATAFORMAT_WAVEFORMATEX) ||
        Format->FormatSize < sizeof(KSDATAFORMAT_WAVEFORMATEX)) {
        return 0;
    }
    if (!IsEqualGUIDAligned(Format->MajorFormat, KSDATAFORMAT_TYPE_AUDIO) ||
        !IsEqualGUIDAligned(Format->Specifier,   KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)) {
        return 0;
    }
    const WAVEFORMATEX* wfx =
        &reinterpret_cast<const KSDATAFORMAT_WAVEFORMATEX*>(Format)->WaveFormatEx;
    BOOLEAN pcm = IsEqualGUIDAligned(Format->SubFormat, KSDATAFORMAT_SUBTYPE_PCM) ? TRUE : FALSE;
    if (wfx->wFormatTag == WAVE_FORMAT_EXTENSIBLE) {
        if (Size < g_FormatBytes || Format->FormatSize < g_FormatBytes ||
            wfx->cbSize < sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)) {
            return 0;
        }
        const WAVEFORMATEXTENSIBLE* ext =
            reinterpret_cast<const WAVEFORMATEXTENSIBLE*>(wfx);
        pcm = (pcm && IsEqualGUIDAligned(ext->SubFormat, KSDATAFORMAT_SUBTYPE_PCM)) ? TRUE : FALSE;
        if (ext->Samples.wValidBitsPerSample != 0 &&
            ext->Samples.wValidBitsPerSample != STREAM_TO_SPEAKER_BITS_PER_SAMPLE) {
            return 0;
        }
    } else if (wfx->wFormatTag != WAVE_FORMAT_PCM) {
        return 0;
    }
    if (!pcm ||
        wfx->nSamplesPerSec != STREAM_TO_SPEAKER_SAMPLE_RATE ||
        wfx->wBitsPerSample != STREAM_TO_SPEAKER_BITS_PER_SAMPLE ||
        (wfx->nChannels != 1 && wfx->nChannels != STREAM_TO_SPEAKER_CHANNELS) ||
        wfx->nBlockAlign != wfx->nChannels * (STREAM_TO_SPEAKER_BITS_PER_SAMPLE / 8u)) {
        return 0;
    }
    return wfx->nChannels;
}

/* ------------------------------------------------------------------ */
/* Data ranges for the wave pin                                        */
/* ------------------------------------------------------------------ */

/* KSDATARANGE_AUDIO has no minimum channel count: MaximumChannels == 2
 * means "1 or 2 channels" (HLK's General Audio Test probes both), so
 * every format path in this file accepts mono as well as stereo. */
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
/* Filter-level property handler — KSPROPSETID_Pin queries that        */
/* AudioEndpointBuilder calls before creating the endpoint:           */
/*   KSPROPERTY_PIN_PROPOSEDATAFORMAT  (SET): validate a proposed fmt  */
/*   KSPROPERTY_PIN_PROPOSEDATAFORMAT2 (GET): default format for a     */
/*                                            signal-processing mode   */
/* PortCls does NOT auto-handle these; if the filter's AutomationTable */
/* is NULL, AEB sees STATUS_NOT_SUPPORTED and won't fully classify the */
/* endpoint (one of the symptoms that leaves us at "Internal AUX Jack" */
/* placeholder). sysvad's speakerwavtable.h pattern.                   */
/* ------------------------------------------------------------------ */
NTSTATUS PropertyHandler_WaveFilter(_In_ PPCPROPERTY_REQUEST Request);

static PCPROPERTY_ITEM PropertiesWaveFilter[] =
{
    {
        &KSPROPSETID_Pin,
        KSPROPERTY_PIN_PROPOSEDATAFORMAT,
        KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT,
        PropertyHandler_WaveFilter
    },
    {
        &KSPROPSETID_Pin,
        KSPROPERTY_PIN_PROPOSEDATAFORMAT2,
        KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_BASICSUPPORT,
        PropertyHandler_WaveFilter
    }
};
DEFINE_PCAUTOMATION_TABLE_PROP(AutomationWaveFilter, PropertiesWaveFilter);

/* ------------------------------------------------------------------ */
/* PCPIN_DESCRIPTOR table                                              */
/* ------------------------------------------------------------------ */

static PCPIN_DESCRIPTOR WaveMiniportPins[] =
{
    /* PIN 0: render sink (data IN from the audio engine). This is a
     * HOST pin (KSPIN_COMMUNICATION_SINK), instantiated by the engine
     * when an app opens the device. Instance counts 1,1,0 are correct
     * here — exactly one engine connection at a time. */
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
    /* PIN 1: bridge (logical out to topology, KSPIN_COMMUNICATION_NONE).
     * Bridge pin: instance counts MUST be 0,0,0 — per Audio Filter
     * Graphs docs, bridge pins exist implicitly and cannot be
     * instantiated. PcRegisterPhysicalConnection still works with
     * 0,0,0 (that's the sysvad / simpleaudiosample pattern); 1,1,0
     * made AEB skip the bridge-pin scan and never properly classify
     * the endpoint. */
    {
        0, 0, 0,
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

/* PCFILTER_DESCRIPTOR.Categories: NULL / 0. The INF's
 * [StreamToSpeaker_Inst.NT.Interfaces] AddInterface lines already
 * register KSCATEGORY_AUDIO / RENDER / REALTIME for the wave filter
 * at PnP level. Double-registering here can confuse AEB's category-
 * matching pass. Both reference samples leave this NULL. */
static PCFILTER_DESCRIPTOR WaveMiniportFilterDescriptor =
{
    0,                                              /* Version          */
    &AutomationWaveFilter,                          /* AutomationTable
                                                      — KSPROPSETID_Pin
                                                      proposed-format
                                                      lives here       */
    sizeof(PCPIN_DESCRIPTOR),                       /* PinSize          */
    SIZEOF_ARRAY(WaveMiniportPins),                 /* PinCount         */
    WaveMiniportPins,                               /* Pins             */
    0,                                              /* NodeSize         */
    0,                                              /* NodeCount        */
    NULL,                                           /* Nodes            */
    SIZEOF_ARRAY(WaveMiniportConnections),          /* ConnectionCount  */
    WaveMiniportConnections,                        /* Connections      */
    0,                                              /* CategoryCount    */
    NULL                                            /* Categories       */
};

/* ------------------------------------------------------------------ */
/* PropertyHandler_WaveFilter implementation                           */
/* ------------------------------------------------------------------ */

/* KSPROPERTY_TYPE_BASICSUPPORT for a property with no value ranges.
 * KS clients may pass either a ULONG (access flags only) or a
 * KSPROPERTY_DESCRIPTION; both must be honoured (HLK KS Topology
 * Test, TC_CheckPropertyDescriptorSize). */
static NTSTATUS BasicSupportNoMembers(
    _In_ PPCPROPERTY_REQUEST Request,
    _In_ ULONG               AccessFlags,
    _In_ ULONG               PropTypeId)
{
    if (Request->ValueSize >= sizeof(KSPROPERTY_DESCRIPTION)) {
        PKSPROPERTY_DESCRIPTION d =
            static_cast<PKSPROPERTY_DESCRIPTION>(Request->Value);
        d->AccessFlags       = AccessFlags;
        d->DescriptionSize   = sizeof(KSPROPERTY_DESCRIPTION);
        d->PropTypeSet.Set   = KSPROPTYPESETID_General;
        d->PropTypeSet.Id    = PropTypeId;
        d->PropTypeSet.Flags = 0;
        d->MembersListCount  = 0;
        d->Reserved          = 0;
        Request->ValueSize   = sizeof(KSPROPERTY_DESCRIPTION);
        return STATUS_SUCCESS;
    }
    if (Request->ValueSize >= sizeof(ULONG)) {
        *static_cast<PULONG>(Request->Value) = AccessFlags;
        Request->ValueSize = sizeof(ULONG);
        return STATUS_SUCCESS;
    }
    Request->ValueSize = 0;
    return STATUS_BUFFER_TOO_SMALL;
}

static NTSTATUS PropertyHandlerProposedFormat(_In_ PPCPROPERTY_REQUEST Request)
{
    PAGED_CODE();
    if (Request->Verb & KSPROPERTY_TYPE_BASICSUPPORT) {
        return BasicSupportNoMembers(Request,
                                     KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT,
                                     VT_ILLEGAL);
    }
    if (!(Request->Verb & KSPROPERTY_TYPE_SET)) {
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    if (Request->ValueSize < sizeof(KSDATAFORMAT_WAVEFORMATEX)) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    const KSDATAFORMAT* fmt = static_cast<const KSDATAFORMAT*>(Request->Value);
    if (StreamToSpeakerSupportedChannels(fmt, Request->ValueSize) == 0) {
        return STATUS_NO_MATCH;
    }
    return STATUS_SUCCESS;
}

/* Locate the signal-processing-mode attribute in a KSMULTIPLE_ITEM
 * attribute list (as attached to KSP_PIN by PROPOSEDATAFORMAT2).
 * Returns STATUS_NOT_FOUND when the list carries no mode attribute. */
static NTSTATUS SignalProcessingModeFromAttributes(
    _In_reads_bytes_(Bytes) const VOID* List,
    _In_  size_t Bytes,
    _Out_ GUID*  Mode)
{
    if (Bytes < sizeof(KSMULTIPLE_ITEM)) {
        return STATUS_NOT_FOUND;
    }
    const KSMULTIPLE_ITEM* items = static_cast<const KSMULTIPLE_ITEM*>(List);
    if (items->Size < sizeof(KSMULTIPLE_ITEM) || items->Size > Bytes) {
        return STATUS_INVALID_PARAMETER;
    }
    const UCHAR* p   = reinterpret_cast<const UCHAR*>(items + 1);
    const UCHAR* end = reinterpret_cast<const UCHAR*>(items) + items->Size;
    for (ULONG i = 0; i < items->Count; ++i) {
        if (p + sizeof(KSATTRIBUTE) > end) {
            return STATUS_INVALID_PARAMETER;
        }
        const KSATTRIBUTE* attr = reinterpret_cast<const KSATTRIBUTE*>(p);
        if (attr->Size < sizeof(KSATTRIBUTE) || p + attr->Size > end) {
            return STATUS_INVALID_PARAMETER;
        }
        if (IsEqualGUIDAligned(attr->Attribute, KSATTRIBUTEID_AUDIOSIGNALPROCESSING_MODE)) {
            if (attr->Size < sizeof(KSATTRIBUTE_AUDIOSIGNALPROCESSING_MODE)) {
                return STATUS_INVALID_PARAMETER;
            }
            *Mode = reinterpret_cast<const KSATTRIBUTE_AUDIOSIGNALPROCESSING_MODE*>(attr)
                        ->SignalProcessingMode;
            return STATUS_SUCCESS;
        }
        /* Attributes are 8-byte aligned within the list. */
        p += (attr->Size + FILE_QUAD_ALIGNMENT) & ~static_cast<size_t>(FILE_QUAD_ALIGNMENT);
    }
    return STATUS_NOT_FOUND;
}

static BOOLEAN IsSupportedSignalProcessingMode(_In_ const GUID& Mode)
{
    for (ULONG i = 0; i < SIZEOF_ARRAY(g_SignalProcessingModes); ++i) {
        if (IsEqualGUIDAligned(Mode, g_SignalProcessingModes[i])) {
            return TRUE;
        }
    }
    return FALSE;
}

/* KSPROPERTY_PIN_PROPOSEDATAFORMAT2 (GET): the instance data is a
 * KSP_PIN followed by an attribute list naming a signal-processing
 * mode; the reply is the pin's default format for that mode with the
 * same attribute list appended (KSDATAFORMAT_ATTRIBUTES). Mirrors
 * sysvad's PropertyHandlerProposedFormat2. */
static NTSTATUS PropertyHandlerProposedFormat2(_In_ PPCPROPERTY_REQUEST Request)
{
    PAGED_CODE();
    if (Request->Verb & KSPROPERTY_TYPE_BASICSUPPORT) {
        return BasicSupportNoMembers(Request,
                                     KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_BASICSUPPORT,
                                     VT_ILLEGAL);
    }
    if (!(Request->Verb & KSPROPERTY_TYPE_GET)) {
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    /* Request->Instance points at KSP_PIN::PinId (everything after the
     * KSPROPERTY header). */
    if (Request->Instance == nullptr ||
        Request->InstanceSize < sizeof(KSP_PIN) - RTL_SIZEOF_THROUGH_FIELD(KSP_PIN, Property)) {
        return STATUS_INVALID_PARAMETER;
    }
    const KSP_PIN* kspPin = CONTAINING_RECORD(Request->Instance, KSP_PIN, PinId);
    if (kspPin->PinId != KSPIN_WAVE_RENDER_SINK) {
        return STATUS_NOT_SUPPORTED;
    }
    const UCHAR* attrs   = reinterpret_cast<const UCHAR*>(kspPin + 1);
    const UCHAR* instEnd = static_cast<const UCHAR*>(Request->Instance) + Request->InstanceSize;
    size_t cbAttrs = (attrs < instEnd) ? static_cast<size_t>(instEnd - attrs) : 0;

    GUID mode = AUDIO_SIGNALPROCESSINGMODE_DEFAULT;
    NTSTATUS status = SignalProcessingModeFromAttributes(attrs, cbAttrs, &mode);
    if (status == STATUS_NOT_FOUND) {
        cbAttrs = 0;
    } else if (!NT_SUCCESS(status)) {
        return status;
    }
    if (!IsSupportedSignalProcessingMode(mode)) {
        return STATUS_NOT_SUPPORTED;
    }

    ULONG cbFormat = (g_FormatBytes + FILE_QUAD_ALIGNMENT) & ~static_cast<ULONG>(FILE_QUAD_ALIGNMENT);
    if (cbAttrs > MAXULONG - cbFormat) {
        return STATUS_INVALID_PARAMETER;
    }
    ULONG cbNeeded = cbFormat + static_cast<ULONG>(cbAttrs);
    if (Request->ValueSize == 0) {
        Request->ValueSize = cbNeeded;
        return STATUS_BUFFER_OVERFLOW;
    }
    if (Request->ValueSize < cbNeeded) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    UCHAR* out = static_cast<UCHAR*>(Request->Value);
    RtlZeroMemory(out, cbNeeded);
    FillFormat(out, STREAM_TO_SPEAKER_CHANNELS);
    if (cbAttrs > 0) {
        reinterpret_cast<PKSDATAFORMAT>(out)->Flags = KSDATAFORMAT_ATTRIBUTES;
        RtlCopyMemory(out + cbFormat, attrs, cbAttrs);
    }
    Request->ValueSize = cbNeeded;
    return STATUS_SUCCESS;
}

NTSTATUS PropertyHandler_WaveFilter(_In_ PPCPROPERTY_REQUEST Request)
{
    PAGED_CODE();
    if (Request == nullptr || Request->PropertyItem == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    if (IsEqualGUIDAligned(*Request->PropertyItem->Set, KSPROPSETID_Pin)) {
        switch (Request->PropertyItem->Id) {
        case KSPROPERTY_PIN_PROPOSEDATAFORMAT:
            return PropertyHandlerProposedFormat(Request);
        case KSPROPERTY_PIN_PROPOSEDATAFORMAT2:
            return PropertyHandlerProposedFormat2(Request);
        default:
            break;
        }
    }
    return STATUS_NOT_FOUND;
}

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
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniportAudioSignalProcessing)) {
        *Object = PVOID(PMINIPORTAudioSignalProcessing(this));
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

/* IMiniportAudioSignalProcessing::GetModes — PortCls serves
 * KSPROPERTY_AUDIOSIGNALPROCESSING_MODES from this. Only the render
 * sink pin is mode-aware; the bridge pin reports "no modes". */
STDMETHODIMP_(NTSTATUS)
CMiniportWaveRT::GetModes(
    _In_                                        ULONG  Pin,
    _Out_writes_opt_(*NumSignalProcessingModes) GUID*  SignalProcessingModes,
    _Inout_                                     ULONG* NumSignalProcessingModes)
{
    PAGED_CODE();
    if (NumSignalProcessingModes == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    if (Pin >= SIZEOF_ARRAY(WaveMiniportPins)) {
        return STATUS_INVALID_PARAMETER;
    }
    if (Pin != KSPIN_WAVE_RENDER_SINK) {
        return STATUS_NOT_SUPPORTED;
    }
    const ULONG count = SIZEOF_ARRAY(g_SignalProcessingModes);
    if (SignalProcessingModes != nullptr) {
        if (*NumSignalProcessingModes < count) {
            *NumSignalProcessingModes = count;
            return STATUS_BUFFER_TOO_SMALL;
        }
        for (ULONG i = 0; i < count; ++i) {
            SignalProcessingModes[i] = g_SignalProcessingModes[i];
        }
    }
    *NumSignalProcessingModes = count;
    return STATUS_SUCCESS;
}

/* Intersect the client's KSDATARANGE_AUDIO with ours. Sample rate and
 * bit depth are fixed; the channel count follows the client (1 or 2),
 * because a data range with MaximumChannels == 2 promises both. */
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
    if (ClientDataRange == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }

    if (PinId != KSPIN_WAVE_RENDER_SINK) {
        return STATUS_NOT_SUPPORTED;
    }

    /* Only KSDATAFORMAT_TYPE_AUDIO + PCM (wildcards allowed). */
    if (!IsEqualGUIDAligned(ClientDataRange->MajorFormat, KSDATAFORMAT_TYPE_AUDIO) &&
        !IsEqualGUIDAligned(ClientDataRange->MajorFormat, KSDATAFORMAT_TYPE_WILDCARD)) {
        return STATUS_NO_MATCH;
    }
    if (!IsEqualGUIDAligned(ClientDataRange->SubFormat, KSDATAFORMAT_SUBTYPE_PCM) &&
        !IsEqualGUIDAligned(ClientDataRange->SubFormat, KSDATAFORMAT_SUBTYPE_WILDCARD)) {
        return STATUS_NO_MATCH;
    }
    if (!IsEqualGUIDAligned(ClientDataRange->Specifier, KSDATAFORMAT_SPECIFIER_WAVEFORMATEX) &&
        !IsEqualGUIDAligned(ClientDataRange->Specifier, KSDATAFORMAT_SPECIFIER_WILDCARD)) {
        return STATUS_NO_MATCH;
    }

    ULONG channels = STREAM_TO_SPEAKER_CHANNELS;
    if (ClientDataRange->FormatSize >= sizeof(KSDATARANGE_AUDIO)) {
        const KSDATARANGE_AUDIO* client =
            reinterpret_cast<const KSDATARANGE_AUDIO*>(ClientDataRange);
        if (client->MinimumSampleFrequency > STREAM_TO_SPEAKER_SAMPLE_RATE ||
            client->MaximumSampleFrequency < STREAM_TO_SPEAKER_SAMPLE_RATE ||
            client->MinimumBitsPerSample   > STREAM_TO_SPEAKER_BITS_PER_SAMPLE ||
            client->MaximumBitsPerSample   < STREAM_TO_SPEAKER_BITS_PER_SAMPLE ||
            client->MaximumChannels        == 0) {
            return STATUS_NO_MATCH;
        }
        if (client->MaximumChannels < STREAM_TO_SPEAKER_CHANNELS) {
            channels = client->MaximumChannels;
        }
    }

    if (OutputBufferLength == 0) {
        *ResultantFormatLength = g_FormatBytes;
        return STATUS_BUFFER_OVERFLOW;
    }
    if (OutputBufferLength < g_FormatBytes) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    if (ResultantFormat == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }

    FillFormat(ResultantFormat, channels);
    *ResultantFormatLength = g_FormatBytes;
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
    if (StreamToSpeakerSupportedChannels(DataFormat, DataFormat->FormatSize) == 0) {
        return STATUS_NO_MATCH;
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
