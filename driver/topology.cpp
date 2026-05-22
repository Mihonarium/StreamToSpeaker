/*
 * topology.cpp - Topology miniport: filter, pins, nodes, properties.
 *
 * Patterned after sysvad's TopologyTable + MiniportTopology. v1 has
 * exactly:
 *   - 1 input pin (from wave miniport)
 *   - 1 output pin (line-out terminator)
 *   - 3 nodes: VOLUME, MUTE, DAC, chained in that order
 *
 * The volume node advertises a per-channel range of -96 dB to 0 dB in
 * 0.5 dB steps. We store the value but DO NOT scale the PCM stream;
 * attenuation is applied externally by the Sonos speaker.
 */

#include "topology.h"

/* ------------------------------------------------------------------ */
/* KSPROPERTY automation tables                                        */
/* ------------------------------------------------------------------ */

/* Forward declaration of the dispatch thunk. */
NTSTATUS PropertyHandler_Topology(_In_ PPCPROPERTY_REQUEST Request);

/* DEFINE_PCAUTOMATION_TABLE_PROP wants an array of PCPROPERTY_ITEM;
 * we define one array per node. Fields are: { Set, Id, Flags, Handler }. */

static PCPROPERTY_ITEM PropertiesVolume[] =
{
    {
        &KSPROPSETID_Audio,
        KSPROPERTY_AUDIO_VOLUMELEVEL,
        KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT,
        PropertyHandler_Topology
    }
};
DEFINE_PCAUTOMATION_TABLE_PROP(AutomationVolume, PropertiesVolume);

static PCPROPERTY_ITEM PropertiesMute[] =
{
    {
        &KSPROPSETID_Audio,
        KSPROPERTY_AUDIO_MUTE,
        KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT,
        PropertyHandler_Topology
    }
};
DEFINE_PCAUTOMATION_TABLE_PROP(AutomationMute, PropertiesMute);

/* ------------------------------------------------------------------ */
/* Topology descriptor tables                                          */
/* ------------------------------------------------------------------ */

static KSDATARANGE TopoPinDataRangesBridge[] =
{
    {
        sizeof(KSDATARANGE),
        0,
        0,
        0,
        STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_ANALOG),
        STATICGUIDOF(KSDATAFORMAT_SPECIFIER_NONE)
    }
};

static PKSDATARANGE TopoPinDataRangePointersBridge[] =
{
    &TopoPinDataRangesBridge[0]
};

static PCPIN_DESCRIPTOR TopologyMiniportPins[] =
{
    /* PIN 0 - input from wave miniport. Bridge pins still need
     * MaxFilter>=1 so PortCls can materialise the connection. */
    {
        1, 1, 0,
        NULL,
        {
            0, NULL, 0, NULL,
            SIZEOF_ARRAY(TopoPinDataRangePointersBridge),
            TopoPinDataRangePointersBridge,
            KSPIN_DATAFLOW_IN,
            KSPIN_COMMUNICATION_NONE,
            &KSCATEGORY_AUDIO,
            NULL,
            0
        }
    },
    /* PIN 1 - output to "line out" (logically: the Sonos).
     * Category is KSCATEGORY_AUDIO; KSNODETYPE_* is for the
     * topology nodes, not for the pin's category slot. */
    {
        1, 1, 0,
        NULL,
        {
            0, NULL, 0, NULL,
            SIZEOF_ARRAY(TopoPinDataRangePointersBridge),
            TopoPinDataRangePointersBridge,
            KSPIN_DATAFLOW_OUT,
            KSPIN_COMMUNICATION_NONE,
            &KSCATEGORY_AUDIO,
            NULL,
            0
        }
    }
};

/* PCNODE_DESCRIPTOR has exactly 4 fields: { Flags, AutomationTable,
 * Type, Name }. */
static PCNODE_DESCRIPTOR TopologyMiniportNodes[] =
{
    /* NODE 0 - VOLUME */
    {
        0,
        &AutomationVolume,
        &KSNODETYPE_VOLUME,
        NULL
    },
    /* NODE 1 - MUTE */
    {
        0,
        &AutomationMute,
        &KSNODETYPE_MUTE,
        NULL
    },
    /* NODE 2 - DAC */
    {
        0,
        NULL,
        &KSNODETYPE_DAC,
        NULL
    }
};

/* Connections: PIN0 -> VOLUME -> MUTE -> DAC -> PIN1. */
static PCCONNECTION_DESCRIPTOR TopologyMiniportConnections[] =
{
    { PCFILTER_NODE,    KSPIN_TOPOLOGY_WAVEOUT_SOURCE, KSNODE_TOPO_VOLUME, 1 },
    { KSNODE_TOPO_VOLUME, 0,                           KSNODE_TOPO_MUTE,   1 },
    { KSNODE_TOPO_MUTE,   0,                           KSNODE_TOPO_DAC,    1 },
    { KSNODE_TOPO_DAC,    0,                           PCFILTER_NODE,
      KSPIN_TOPOLOGY_LINEOUT_DEST }
};

static const GUID TopologyMiniportCategories[] =
{
    STATICGUIDOF(KSCATEGORY_AUDIO),
    STATICGUIDOF(KSCATEGORY_RENDER)
};

static PCFILTER_DESCRIPTOR TopologyMiniportFilterDescriptor =
{
    0,                                              /* Version          */
    NULL,                                           /* AutomationTable  */
    sizeof(PCPIN_DESCRIPTOR),                       /* PinSize          */
    SIZEOF_ARRAY(TopologyMiniportPins),             /* PinCount         */
    TopologyMiniportPins,                           /* Pins             */
    sizeof(PCNODE_DESCRIPTOR),                      /* NodeSize         */
    SIZEOF_ARRAY(TopologyMiniportNodes),            /* NodeCount        */
    TopologyMiniportNodes,                          /* Nodes            */
    SIZEOF_ARRAY(TopologyMiniportConnections),      /* ConnectionCount  */
    TopologyMiniportConnections,                    /* Connections      */
    SIZEOF_ARRAY(TopologyMiniportCategories),       /* CategoryCount    */
    TopologyMiniportCategories                      /* Categories       */
};

const KSFILTER_DESCRIPTOR*
StreamToSpeakerTopologyFilterDescriptor()
{
    /* PortCls uses PCFILTER_DESCRIPTOR for IMiniportTopology; the
     * accessor returns nullptr because callers that need a KS
     * descriptor go through GetDescription. Kept for symmetry with
     * sysvad. */
    return nullptr;
}

/* ------------------------------------------------------------------ */
/* Property handler thunk                                              */
/* ------------------------------------------------------------------ */

NTSTATUS
PropertyHandler_Topology(_In_ PPCPROPERTY_REQUEST Request)
{
    PAGED_CODE();
    if (Request == nullptr || Request->MajorTarget == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    CMiniportTopology* topo = static_cast<CMiniportTopology*>(
        static_cast<PMINIPORTTOPOLOGY>(Request->MajorTarget));
    if (topo == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    if (Request->PropertyItem == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }

    if (IsEqualGUIDAligned(*Request->PropertyItem->Set, KSPROPSETID_Audio)) {
        switch (Request->PropertyItem->Id) {
        case KSPROPERTY_AUDIO_VOLUMELEVEL:
            return topo->PropertyHandlerVolumeLevel(Request);
        case KSPROPERTY_AUDIO_MUTE:
            return topo->PropertyHandlerMute(Request);
        default:
            break;
        }
    } else if (IsEqualGUIDAligned(*Request->PropertyItem->Set,
                                  KSPROPSETID_General)) {
        if (Request->PropertyItem->Id == KSPROPERTY_GENERAL_COMPONENTID) {
            /* Generic component ID is handled by PortCls; we leave it
             * for the default. */
            return STATUS_NOT_FOUND;
        }
    }
    return STATUS_NOT_FOUND;
}

/* ------------------------------------------------------------------ */
/* CMiniportTopology                                                   */
/* ------------------------------------------------------------------ */

NTSTATUS
CreateMiniportTopologyStreamToSpeaker(
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
    CMiniportTopology* p = new (PoolFlags, STREAM_TO_SPEAKER_POOL_TAG)
        CMiniportTopology(OuterUnknown);
    if (p == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    *Unknown = PUNKNOWN((PMINIPORTTOPOLOGY)p);
    (*Unknown)->AddRef();
    return STATUS_SUCCESS;
}

CMiniportTopology::~CMiniportTopology()
{
    if (m_PortEvents != nullptr) {
        m_PortEvents->Release();
        m_PortEvents = nullptr;
    }
    if (m_Port != nullptr) {
        m_Port->Release();
        m_Port = nullptr;
    }
    if (m_Ext != nullptr) {
        if (m_Ext->Topology == this) {
            m_Ext->Topology = nullptr;
        }
        m_Ext = nullptr;
    }
}

STDMETHODIMP
CMiniportTopology::NonDelegatingQueryInterface(
    _In_ REFIID Interface,
    _COM_Outptr_ PVOID* Object)
{
    PAGED_CODE();
    if (Object == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    if (IsEqualGUIDAligned(Interface, IID_IUnknown)) {
        *Object = PVOID(PUNKNOWN(PMINIPORTTOPOLOGY(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniport) ||
               IsEqualGUIDAligned(Interface, IID_IMiniportTopology)) {
        *Object = PVOID(PMINIPORTTOPOLOGY(this));
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
CMiniportTopology::GetDescription(_Out_ PPCFILTER_DESCRIPTOR* OutFilterDescriptor)
{
    PAGED_CODE();
    if (OutFilterDescriptor == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    *OutFilterDescriptor = &TopologyMiniportFilterDescriptor;
    return STATUS_SUCCESS;
}

STDMETHODIMP
CMiniportTopology::DataRangeIntersection(
    _In_  ULONG               PinId,
    _In_  PKSDATARANGE        DataRange,
    _In_  PKSDATARANGE        MatchingDataRange,
    _In_  ULONG               OutputBufferLength,
    _Out_writes_bytes_to_opt_(OutputBufferLength, *ResultantFormatLength) PVOID ResultantFormat,
    _Out_ PULONG              ResultantFormatLength)
{
    UNREFERENCED_PARAMETER(PinId);
    UNREFERENCED_PARAMETER(DataRange);
    UNREFERENCED_PARAMETER(MatchingDataRange);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(ResultantFormat);
    if (ResultantFormatLength != nullptr) {
        *ResultantFormatLength = 0;
    }
    return STATUS_NOT_IMPLEMENTED;
}

STDMETHODIMP
CMiniportTopology::Init(
    _In_ PUNKNOWN     UnknownAdapter,
    _In_ PRESOURCELIST ResourceList,
    _In_ PPORTTOPOLOGY Port)
{
    UNREFERENCED_PARAMETER(ResourceList);
    PAGED_CODE();
    /* UnknownAdapter may be nullptr for a virtual driver — we have no
     * adapter common object. Only Port is required. */
    if (Port == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }

    m_Port = Port;
    m_Port->AddRef();
    m_UnknownAdapter = UnknownAdapter;  /* may be nullptr */

    /* Query IPortEvents so we can notify clients of volume/mute changes. */
    m_PortEvents = nullptr;
    NTSTATUS status = Port->QueryInterface(
        IID_IPortEvents,
        reinterpret_cast<PVOID*>(&m_PortEvents));
    if (!NT_SUCCESS(status)) {
        /* Non-fatal: we just won't fire change events. */
        m_PortEvents = nullptr;
    }

    for (ULONG i = 0; i < STREAM_TO_SPEAKER_CHANNELS; ++i) {
        m_VolumeMb[i] = STREAM_TO_SPEAKER_VOLUME_DEFAULT_MILLIBELS;
    }
    m_Muted = FALSE;
    KeInitializeSpinLock(&m_ControlLock);

    return STATUS_SUCCESS;
}

/* ------------------------------------------------------------------ */
/* Property handlers                                                   */
/* ------------------------------------------------------------------ */

static VOID FillVolumeBasicSupport(
    _Out_ PKSPROPERTY_DESCRIPTION desc,
    _Out_ PKSPROPERTY_MEMBERSHEADER hdr,
    _Out_ PKSPROPERTY_STEPPING_LONG step)
{
    desc->AccessFlags       = KSPROPERTY_TYPE_BASICSUPPORT |
                              KSPROPERTY_TYPE_GET |
                              KSPROPERTY_TYPE_SET;
    desc->DescriptionSize   = sizeof(*desc) + sizeof(*hdr) + sizeof(*step);
    desc->PropTypeSet.Set   = KSPROPTYPESETID_General;
    desc->PropTypeSet.Id    = VT_I4;
    desc->PropTypeSet.Flags = 0;
    desc->MembersListCount  = 1;
    desc->Reserved          = 0;

    hdr->MembersFlags       = KSPROPERTY_MEMBER_STEPPEDRANGES;
    hdr->MembersSize        = sizeof(*step);
    hdr->MembersCount       = 1;
    hdr->Flags              = KSPROPERTY_MEMBER_FLAG_BASICSUPPORT_UNIFORM;

    step->SteppingDelta     = STREAM_TO_SPEAKER_VOLUME_STEP_MILLIBELS;
    step->Reserved          = 0;
    step->Bounds.SignedMinimum = STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS;
    step->Bounds.SignedMaximum = STREAM_TO_SPEAKER_VOLUME_MAX_MILLIBELS;
}

NTSTATUS
CMiniportTopology::PropertyHandlerVolumeLevel(_In_ PPCPROPERTY_REQUEST Request)
{
    PAGED_CODE();

    if (Request->Verb & KSPROPERTY_TYPE_BASICSUPPORT) {
        if (Request->ValueSize < sizeof(KSPROPERTY_DESCRIPTION)) {
            return STATUS_BUFFER_TOO_SMALL;
        }
        PKSPROPERTY_DESCRIPTION desc =
            static_cast<PKSPROPERTY_DESCRIPTION>(Request->Value);
        if (Request->ValueSize < sizeof(*desc) +
                                 sizeof(KSPROPERTY_MEMBERSHEADER) +
                                 sizeof(KSPROPERTY_STEPPING_LONG)) {
            /* Return just the size. */
            desc->AccessFlags     = KSPROPERTY_TYPE_BASICSUPPORT |
                                    KSPROPERTY_TYPE_GET |
                                    KSPROPERTY_TYPE_SET;
            desc->DescriptionSize = sizeof(*desc) +
                                    sizeof(KSPROPERTY_MEMBERSHEADER) +
                                    sizeof(KSPROPERTY_STEPPING_LONG);
            desc->PropTypeSet.Set   = KSPROPTYPESETID_General;
            desc->PropTypeSet.Id    = VT_I4;
            desc->PropTypeSet.Flags = 0;
            desc->MembersListCount  = 1;
            desc->Reserved          = 0;
            Request->ValueSize      = sizeof(*desc);
            return STATUS_SUCCESS;
        }
        PKSPROPERTY_MEMBERSHEADER hdr =
            reinterpret_cast<PKSPROPERTY_MEMBERSHEADER>(desc + 1);
        PKSPROPERTY_STEPPING_LONG step =
            reinterpret_cast<PKSPROPERTY_STEPPING_LONG>(hdr + 1);
        FillVolumeBasicSupport(desc, hdr, step);
        Request->ValueSize = desc->DescriptionSize;
        return STATUS_SUCCESS;
    }

    /* Channel selector: -1 == "all". */
    if (Request->InstanceSize < sizeof(LONG)) {
        return STATUS_INVALID_PARAMETER;
    }
    LONG channel = *static_cast<LONG*>(Request->Instance);

    if (Request->Verb & KSPROPERTY_TYPE_GET) {
        if (Request->ValueSize < sizeof(LONG)) {
            Request->ValueSize = sizeof(LONG);
            return STATUS_BUFFER_OVERFLOW;
        }
        KIRQL old;
        KeAcquireSpinLock(&m_ControlLock, &old);
        LONG out = (channel < 0 || (ULONG)channel >= STREAM_TO_SPEAKER_CHANNELS)
            ? m_VolumeMb[0]
            : m_VolumeMb[channel];
        KeReleaseSpinLock(&m_ControlLock, old);
        *static_cast<LONG*>(Request->Value) = out;
        Request->ValueSize = sizeof(LONG);
        return STATUS_SUCCESS;
    }

    if (Request->Verb & KSPROPERTY_TYPE_SET) {
        if (Request->ValueSize < sizeof(LONG)) {
            return STATUS_INVALID_PARAMETER;
        }
        LONG val = *static_cast<LONG*>(Request->Value);
        if (val < STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS) val = STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS;
        if (val > STREAM_TO_SPEAKER_VOLUME_MAX_MILLIBELS) val = STREAM_TO_SPEAKER_VOLUME_MAX_MILLIBELS;
        KIRQL old;
        KeAcquireSpinLock(&m_ControlLock, &old);
        if (channel < 0) {
            for (ULONG i = 0; i < STREAM_TO_SPEAKER_CHANNELS; ++i) {
                m_VolumeMb[i] = val;
            }
        } else if ((ULONG)channel < STREAM_TO_SPEAKER_CHANNELS) {
            m_VolumeMb[channel] = val;
        }
        PostVolumeEvent_NoLock();
        KeReleaseSpinLock(&m_ControlLock, old);
        return STATUS_SUCCESS;
    }

    return STATUS_INVALID_DEVICE_REQUEST;
}

NTSTATUS
CMiniportTopology::PropertyHandlerMute(_In_ PPCPROPERTY_REQUEST Request)
{
    PAGED_CODE();

    if (Request->Verb & KSPROPERTY_TYPE_BASICSUPPORT) {
        if (Request->ValueSize < sizeof(KSPROPERTY_DESCRIPTION)) {
            return STATUS_BUFFER_TOO_SMALL;
        }
        PKSPROPERTY_DESCRIPTION desc =
            static_cast<PKSPROPERTY_DESCRIPTION>(Request->Value);
        desc->AccessFlags     = KSPROPERTY_TYPE_BASICSUPPORT |
                                KSPROPERTY_TYPE_GET |
                                KSPROPERTY_TYPE_SET;
        desc->DescriptionSize = sizeof(*desc);
        desc->PropTypeSet.Set   = KSPROPTYPESETID_General;
        desc->PropTypeSet.Id    = VT_BOOL;
        desc->PropTypeSet.Flags = 0;
        desc->MembersListCount  = 0;
        desc->Reserved          = 0;
        Request->ValueSize = sizeof(*desc);
        return STATUS_SUCCESS;
    }

    if (Request->Verb & KSPROPERTY_TYPE_GET) {
        if (Request->ValueSize < sizeof(BOOL)) {
            Request->ValueSize = sizeof(BOOL);
            return STATUS_BUFFER_OVERFLOW;
        }
        KIRQL old;
        KeAcquireSpinLock(&m_ControlLock, &old);
        *static_cast<BOOL*>(Request->Value) = m_Muted ? TRUE : FALSE;
        KeReleaseSpinLock(&m_ControlLock, old);
        Request->ValueSize = sizeof(BOOL);
        return STATUS_SUCCESS;
    }

    if (Request->Verb & KSPROPERTY_TYPE_SET) {
        if (Request->ValueSize < sizeof(BOOL)) {
            return STATUS_INVALID_PARAMETER;
        }
        BOOL val = *static_cast<BOOL*>(Request->Value);
        KIRQL old;
        KeAcquireSpinLock(&m_ControlLock, &old);
        m_Muted = (val != FALSE);
        PostMuteEvent_NoLock();
        KeReleaseSpinLock(&m_ControlLock, old);
        return STATUS_SUCCESS;
    }

    return STATUS_INVALID_DEVICE_REQUEST;
}

NTSTATUS
CMiniportTopology::PropertyHandlerCpuResources(_In_ PPCPROPERTY_REQUEST Request)
{
    PAGED_CODE();
    if (Request->Verb & KSPROPERTY_TYPE_GET) {
        if (Request->ValueSize < sizeof(LONG)) {
            Request->ValueSize = sizeof(LONG);
            return STATUS_BUFFER_OVERFLOW;
        }
        *static_cast<LONG*>(Request->Value) = KSAUDIO_CPU_RESOURCES_NOT_HOST_CPU;
        Request->ValueSize = sizeof(LONG);
        return STATUS_SUCCESS;
    }
    return STATUS_NOT_SUPPORTED;
}

/* ------------------------------------------------------------------ */
/* Event posting helpers                                               */
/* ------------------------------------------------------------------ */

VOID
CMiniportTopology::PostVolumeEvent_NoLock()
{
    if (m_Ext == nullptr || m_Ext->IoctlCtx == nullptr) {
        return;
    }
    STREAM_TO_SPEAKER_CONTROL_EVENT ev = { };
    ev.EventType = StreamToSpeakerEventVolumeChanged;
    /* Report the loudest channel (typically L == R from the Windows
     * mixer; per-channel only really happens via SetMute calls). */
    LONG mb = m_VolumeMb[0];
    for (ULONG i = 1; i < STREAM_TO_SPEAKER_CHANNELS; ++i) {
        if (m_VolumeMb[i] > mb) {
            mb = m_VolumeMb[i];
        }
    }
    ev.Data.Volume.LevelMillibels = mb;
    IoctlPostEvent(m_Ext->IoctlCtx, &ev);
}

VOID
CMiniportTopology::PostMuteEvent_NoLock()
{
    if (m_Ext == nullptr || m_Ext->IoctlCtx == nullptr) {
        return;
    }
    STREAM_TO_SPEAKER_CONTROL_EVENT ev = { };
    ev.EventType = StreamToSpeakerEventMuteChanged;
    ev.Data.Mute.Muted = m_Muted ? 1 : 0;
    IoctlPostEvent(m_Ext->IoctlCtx, &ev);
}

VOID
CMiniportTopology::FireControlChange(_In_ ULONG NodeId)
{
    if (m_PortEvents == nullptr) {
        return;
    }
    /* Fire a KSEVENT_CONTROL_CHANGE so Windows re-queries the node's
     * properties (volume, mute) and updates the mixer UI. Matches
     * sysvad's EvtSpeakerVolumeHandler pattern. */
    m_PortEvents->GenerateEventList(
        const_cast<GUID*>(&KSEVENTSETID_AudioControlChange),
        KSEVENT_CONTROL_CHANGE,
        FALSE,         /* not a pin event */
        ULONG(-1),     /* pin id unused */
        TRUE,          /* node event */
        NodeId);
}

VOID
CMiniportTopology::OnExternalVolumeChange(_In_ INT32 Mb, _In_ BOOLEAN Muted)
{
    KIRQL old;
    KeAcquireSpinLock(&m_ControlLock, &old);
    for (ULONG i = 0; i < STREAM_TO_SPEAKER_CHANNELS; ++i) {
        m_VolumeMb[i] = Mb;
    }
    m_Muted = Muted;
    KeReleaseSpinLock(&m_ControlLock, old);
    FireControlChange(KSNODE_TOPO_VOLUME);
    FireControlChange(KSNODE_TOPO_MUTE);
}

/* Free function used by ioctl.cpp through extern declaration. */
VOID
TopologyOnExternalVolumeChange(
    _In_ CMiniportTopology* Topology,
    _In_ INT32              LevelMillibels,
    _In_ BOOLEAN            Muted)
{
    if (Topology != nullptr) {
        Topology->OnExternalVolumeChange(LevelMillibels, Muted);
    }
}
