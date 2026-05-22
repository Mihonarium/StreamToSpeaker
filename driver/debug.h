/*
 * debug.h - DbgPrint verbosity wrappers used throughout the driver.
 *
 * Verbosity levels follow the convention:
 *   ERROR    - always printed
 *   WARN     - serious but recoverable
 *   INFO     - lifecycle events (PnP, stream start/stop)
 *   TRACE    - per-call entry/exit (off by default in release builds)
 *   VERBOSE  - per-packet diagnostics (off unless debugging)
 *
 * All macros expand to nothing in MIN_TRACE builds where the level is
 * below the threshold. We avoid variadic-macro tricks beyond what MSVC
 * supports comfortably.
 */

#pragma once

#ifndef _KERNEL_MODE
  /* Allow the file to be parsed by g++/clang outside the WDK for
   * syntax checks (EMULATE_WINDOWS_KERNEL=1 path). */
  #ifndef EMULATE_WINDOWS_KERNEL
    #error "debug.h is for kernel-mode use only"
  #endif
#endif

#define STREAM_TO_SPEAKER_DBG_ERROR    0
#define STREAM_TO_SPEAKER_DBG_WARN     1
#define STREAM_TO_SPEAKER_DBG_INFO     2
#define STREAM_TO_SPEAKER_DBG_TRACE    3
#define STREAM_TO_SPEAKER_DBG_VERBOSE  4

#ifndef STREAM_TO_SPEAKER_DBG_LEVEL
  /* While bringing the driver up, keep INFO+ in Release too. Drop back
     to STREAM_TO_SPEAKER_DBG_WARN once load is reliable. */
  #define STREAM_TO_SPEAKER_DBG_LEVEL    STREAM_TO_SPEAKER_DBG_TRACE
#endif

/* Use the DPFLTR mechanism so messages survive even when no kernel
 * debugger is attached but tracelog is. */
#ifdef EMULATE_WINDOWS_KERNEL
  #include <stdio.h>
  #define STREAM_TO_SPEAKER_DBGPRINT(prefix, fmt, ...) \
      do { fprintf(stderr, "[StreamToSpeaker][" prefix "] " fmt "\n", ##__VA_ARGS__); } while (0)
#else
  #define STREAM_TO_SPEAKER_DBGPRINT(prefix, fmt, ...) \
      DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_INFO_LEVEL, \
                 "[StreamToSpeaker][" prefix "] " fmt "\n", ##__VA_ARGS__)
#endif

#define DBG_ERROR(fmt, ...) \
    do { if (STREAM_TO_SPEAKER_DBG_LEVEL >= STREAM_TO_SPEAKER_DBG_ERROR) \
         STREAM_TO_SPEAKER_DBGPRINT("ERR ", fmt, ##__VA_ARGS__); } while (0)
#define DBG_WARN(fmt, ...) \
    do { if (STREAM_TO_SPEAKER_DBG_LEVEL >= STREAM_TO_SPEAKER_DBG_WARN) \
         STREAM_TO_SPEAKER_DBGPRINT("WARN", fmt, ##__VA_ARGS__); } while (0)
#define DBG_INFO(fmt, ...) \
    do { if (STREAM_TO_SPEAKER_DBG_LEVEL >= STREAM_TO_SPEAKER_DBG_INFO) \
         STREAM_TO_SPEAKER_DBGPRINT("INFO", fmt, ##__VA_ARGS__); } while (0)
#define DBG_TRACE(fmt, ...) \
    do { if (STREAM_TO_SPEAKER_DBG_LEVEL >= STREAM_TO_SPEAKER_DBG_TRACE) \
         STREAM_TO_SPEAKER_DBGPRINT("TRC ", fmt, ##__VA_ARGS__); } while (0)
#define DBG_VERBOSE(fmt, ...) \
    do { if (STREAM_TO_SPEAKER_DBG_LEVEL >= STREAM_TO_SPEAKER_DBG_VERBOSE) \
         STREAM_TO_SPEAKER_DBGPRINT("VRB ", fmt, ##__VA_ARGS__); } while (0)

#define DBG_ENTER()  DBG_TRACE("%s: enter", __FUNCTION__)
#define DBG_LEAVE()  DBG_TRACE("%s: leave", __FUNCTION__)
