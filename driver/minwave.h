/*
 * minwave.h - Wave filter descriptor and pin layout.
 *
 * Pin layout (single render endpoint):
 *
 *   PIN 0  KSPIN_WAVE_RENDER_SINK   data IN  (data flow into us from
 *                                              the Windows audio engine
 *                                              over KS streaming)
 *   PIN 1  KSPIN_WAVE_RENDER_SOURCE data OUT (logical, bridges to the
 *                                              topology filter)
 *
 * Data formats advertised: L16 / 44.1 kHz / stereo only.
 */

#pragma once

#include "driver.h"

#define KSPIN_WAVE_RENDER_SINK    0
#define KSPIN_WAVE_RENDER_SOURCE  1

/* Returns the wave filter PCFILTER_DESCRIPTOR pointer. Owned by
 * wave.cpp static data. */
extern const PCFILTER_DESCRIPTOR* StreamToSpeakerWaveFilterDescriptor();

/* Standard L16/44.1k/stereo waveformatex used by data-range, pin
 * connection negotiation, and the WaveRT cyclic buffer allocator. */
extern const WAVEFORMATEXTENSIBLE* StreamToSpeakerWaveFormat();
