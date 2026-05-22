/*
 * stream_to_speaker_ioctl.h - Shared IOCTL contract between the kernel driver
 *                     (C++/PortCls) and the user-mode bridge service
 *                     (Rust, via DeviceIoControl through windows-sys).
 *
 * This file is the single source of truth for the kernel<->user-mode ABI.
 * It is included in both builds:
 *   - kernel: defines _KERNEL_MODE before include
 *   - user:   plain Windows SDK include
 *
 * All structures are POD with explicit sizes; no compiler-specific padding
 * tricks. Layout must match exactly on x64 (which is the only target).
 */

#pragma once

#ifdef _KERNEL_MODE
  #include <ntddk.h>
#else
  #include <windows.h>
  #include <stdint.h>
  typedef int8_t   INT8;
  typedef uint8_t  UINT8;
  typedef int16_t  INT16;
  typedef uint16_t UINT16;
  typedef int32_t  INT32;
  typedef uint32_t UINT32;
  typedef int64_t  INT64;
  typedef uint64_t UINT64;
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* -----------------------------------------------------------------------
 * Device identification
 * --------------------------------------------------------------------- */

/* Device interface GUID. Used by the user-mode service to find the
 * driver via SetupDiGetClassDevs(GUID_DEVINTERFACE_STREAM_TO_SPEAKER, ...).
 * Generated once; do not change.
 *  {7B3F1F2C-A1A2-4567-89AB-CDEF01234567}
 */
DEFINE_GUID(GUID_DEVINTERFACE_STREAM_TO_SPEAKER,
    0x7b3f1f2c, 0xa1a2, 0x4567, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67);

/* Convenience symbolic link the driver also creates so the user-mode
 * service can CreateFileW(STREAM_TO_SPEAKER_DEVICE_PATH, ...) directly without
 * device enumeration. The device interface GUID is preferred.
 *
 * NB: the constant name retains the historical "STREAM_TO_SPEAKER" prefix for
 * source-stability across this rename — only the string literal points
 * at the new symbolic link name. */
#define STREAM_TO_SPEAKER_DEVICE_PATH    L"\\\\.\\StreamToSpeaker"

/* -----------------------------------------------------------------------
 * IOCTL codes
 * --------------------------------------------------------------------- */

#define STREAM_TO_SPEAKER_IOCTL_BASE     0x800

/* Pended IOCTL. Completes with the next available audio packet
 * (STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER followed by PCM bytes).
 * METHOD_OUT_DIRECT so we hand the user buffer to the driver without
 * extra copies; the driver writes header+PCM directly into it. */
#define IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET                                  \
    CTL_CODE(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 1,               \
             METHOD_OUT_DIRECT, FILE_READ_ACCESS)

/* Pended IOCTL. Completes when a control-plane event is available
 * (volume change initiated by Windows mixer, stream start/stop). */
#define IOCTL_STREAM_TO_SPEAKER_GET_CONTROL_EVENT                                 \
    CTL_CODE(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 2,               \
             METHOD_BUFFERED, FILE_READ_ACCESS)

/* Synchronous IOCTL. Tells the driver about external state changes,
 * e.g. user adjusted volume on the Sonos physically and we need
 * Windows UI to reflect it. */
#define IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME                                       \
    CTL_CODE(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 3,               \
             METHOD_BUFFERED, FILE_WRITE_ACCESS)

/* Synchronous IOCTL. Returns driver/protocol version so we can refuse
 * to talk to a mismatched binary. */
#define IOCTL_STREAM_TO_SPEAKER_GET_VERSION                                       \
    CTL_CODE(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 4,               \
             METHOD_BUFFERED, FILE_READ_ACCESS)

/* -----------------------------------------------------------------------
 * Protocol version
 * --------------------------------------------------------------------- */

#define STREAM_TO_SPEAKER_PROTOCOL_VERSION   1u

typedef struct _STREAM_TO_SPEAKER_VERSION_INFO {
    UINT32 ProtocolVersion;   /* STREAM_TO_SPEAKER_PROTOCOL_VERSION */
    UINT32 DriverBuild;       /* monotonic build number     */
} STREAM_TO_SPEAKER_VERSION_INFO;

/* -----------------------------------------------------------------------
 * Audio packet
 * --------------------------------------------------------------------- */

/* Flags in STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER.Flags */
#define STREAM_TO_SPEAKER_PACKET_FLAG_STREAM_RESTART  0x00000001u
/* Driver hint that this packet is entirely zero. Service may skip
 * silence detection if this is set. Not authoritative — service
 * should still verify if it cares. */
#define STREAM_TO_SPEAKER_PACKET_FLAG_HINT_SILENT     0x00000002u

typedef struct _STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER {
    UINT32 SampleRate;        /* e.g. 44100. Driver advertises one format
                                 per stream session; user-mode service
                                 must handle changes via STREAM_RESTART. */
    UINT16 BitsPerSample;     /* 16 (only value supported in v1)         */
    UINT16 Channels;          /* 2  (only value supported in v1)         */
    UINT32 SampleFrameCount;  /* number of frames = number of L+R pairs  */
    UINT32 DataBytes;         /* PCM bytes following this header.
                                 == SampleFrameCount * Channels * (BitsPerSample/8) */
    UINT32 Flags;             /* STREAM_TO_SPEAKER_PACKET_FLAG_*                 */
    UINT64 TimestampQpc;      /* QueryPerformanceCounter value at the
                                 instant the first frame entered the
                                 WaveRT cyclic buffer. Used for jitter
                                 measurement; not used for playback. */
    UINT64 StreamPosition;    /* Cumulative frame count since stream
                                 started (incl. this packet's frames).
                                 Wraps at 2^64 = ~13 million years @ 48kHz. */
} STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER;

/* Maximum packet size the driver will ever emit. The user-mode service
 * should size its IOCTL output buffer to at least this many bytes plus
 * sizeof(STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER). At 44.1kHz/16/stereo a 10 ms
 * packet is 1764 frames * 4 bytes = 7056 bytes; we round up. */
#define STREAM_TO_SPEAKER_MAX_PACKET_BYTES   8192u
#define STREAM_TO_SPEAKER_IOCTL_BUFFER_BYTES (sizeof(STREAM_TO_SPEAKER_AUDIO_PACKET_HEADER) + STREAM_TO_SPEAKER_MAX_PACKET_BYTES)

/* -----------------------------------------------------------------------
 * Control events
 * --------------------------------------------------------------------- */

typedef enum _STREAM_TO_SPEAKER_EVENT_TYPE {
    StreamToSpeakerEventVolumeChanged = 1,  /* Windows mixer changed volume    */
    StreamToSpeakerEventMuteChanged   = 2,  /* Windows mixer toggled mute      */
    StreamToSpeakerEventStreamStart   = 3,  /* first client opened for output  */
    StreamToSpeakerEventStreamStop    = 4,  /* last client closed              */
    StreamToSpeakerEventFormatChange  = 5,  /* stream format renegotiated      */
} STREAM_TO_SPEAKER_EVENT_TYPE;

typedef struct _STREAM_TO_SPEAKER_CONTROL_EVENT {
    UINT32 EventType;         /* STREAM_TO_SPEAKER_EVENT_TYPE                    */
    UINT32 Reserved;
    union {
        struct {
            /* Volume in millibels. 0 = unity gain. -10000 = -100 dB.
             * Range: [-10000, 0]. Matches Windows IAudioEndpointVolume
             * and UPnP RenderingControl is roughly log-mapped from here. */
            INT32 LevelMillibels;
        } Volume;
        struct {
            UINT32 Muted;     /* 0 or 1                                  */
        } Mute;
        struct {
            UINT32 NewSampleRate;
            UINT16 NewBitsPerSample;
            UINT16 NewChannels;
        } FormatChange;
        UINT8 Padding[16];    /* keeps total size stable across variants */
    } Data;
} STREAM_TO_SPEAKER_CONTROL_EVENT;

/* -----------------------------------------------------------------------
 * Push: user-mode to driver
 * --------------------------------------------------------------------- */

typedef struct _STREAM_TO_SPEAKER_PUSH_VOLUME_INPUT {
    INT32  LevelMillibels;    /* desired master volume                   */
    UINT32 Muted;             /* 0 or 1                                  */
} STREAM_TO_SPEAKER_PUSH_VOLUME_INPUT;

#ifdef __cplusplus
} /* extern "C" */
#endif
