/*
 * mintopo.h - Topology filter / pin / node descriptors.
 *
 * Topology graph (left-to-right, signal flow):
 *
 *     [PIN 0: from wave]  ->  VOLUME  ->  MUTE  ->  DAC  ->  [PIN 1: to speakers]
 *
 * KS PINs:
 *   PIN 0  KSPIN_TOPOLOGY_WAVEOUT_SOURCE
 *          category KSNODETYPE_ANY, communication NONE, direction OUT-of-filter
 *          (data flows IN to this pin from the wave filter).
 *   PIN 1  KSPIN_TOPOLOGY_LINEOUT_DEST
 *          category KSNODETYPE_LINE_CONNECTOR (we present as line-out;
 *          StreamToSpeaker is a network endpoint, but no node type matches
 *          "wireless speaker" cleanly; line-out is what sysvad uses).
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
