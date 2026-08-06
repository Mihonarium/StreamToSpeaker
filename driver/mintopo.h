/*
 * mintopo.h - Topology filter / pin / node descriptors.
 *
 * Topology graph (left-to-right, signal flow):
 *
 *     [PIN 0: from wave]  ->  VOLUME  ->  MUTE  ->  DAC  ->  [PIN 1: to speakers]
 *
 * KS PINs:
 *   PIN 0  KSPIN_TOPOLOGY_WAVEOUT_SOURCE
 *          category KSCATEGORY_AUDIO, communication NONE; data flows IN
 *          to this pin from the wave filter.
 *   PIN 1  KSPIN_TOPOLOGY_LINEOUT_DEST
 *          category KSNODETYPE_SPEAKER. The AudioEndpointBuilder reads
 *          this GUID to derive the form factor; KSNODETYPE_SPEAKER →
 *          "Speakers", which is enabled+visible by default.
 *          KSCATEGORY_AUDIO would yield UnknownFormFactor, which Windows
 *          creates as disabled+hidden (forcing the user to flip the
 *          "Allow apps and Windows to use this device" toggle).
 *
 * KS NODEs:
 *   NODE 0 KSNODETYPE_VOLUME
 *   NODE 1 KSNODETYPE_MUTE
 *   NODE 2 KSNODETYPE_DAC
 */

#pragma once

#include "driver.h"

/* Topology pin indices. */
#define KSPIN_TOPOLOGY_WAVEOUT_SOURCE   0
#define KSPIN_TOPOLOGY_LINEOUT_DEST     1

/* Topology node indices. */
#define KSNODE_TOPO_VOLUME              0
#define KSNODE_TOPO_MUTE                1
#define KSNODE_TOPO_DAC                 2

/* Filter descriptor accessor (exported by topology.cpp). */
extern const KSFILTER_DESCRIPTOR* StreamToSpeakerTopologyFilterDescriptor();

/* Forward decl of the topology miniport class. */
class CMiniportTopology;

/* Hooks used by ioctl.cpp / wavestream.cpp. */
VOID TopologyOnExternalVolumeChange(
    _In_ CMiniportTopology* Topology,
    _In_ INT32              LevelMillibels,
    _In_ BOOLEAN            Muted);
