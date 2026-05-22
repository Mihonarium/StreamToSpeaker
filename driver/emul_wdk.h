/*
 * emul_wdk.h - SHIM ONLY. Not used by the real build.
 *
 * Minimal Windows kernel typedef shim so g++/clang can syntax-check
 * the simpler driver files (ringbuffer, ioctl) without the WDK. The
 * PortCls/KS bits are intentionally incomplete; wave.cpp /
 * wavestream.cpp / topology.cpp / adapter.cpp / driver.cpp will NOT
 * parse under the shim — they need the real WDK headers.
 */

#pragma once

#ifndef EMULATE_WINDOWS_KERNEL
#error emul_wdk.h is only for the syntax-check path
#endif

#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>

/* ---- basic types ---- */
typedef void              VOID;
typedef void*             PVOID;
typedef unsigned char     UCHAR;
typedef unsigned char     BOOLEAN;
typedef unsigned char*    PUCHAR;
typedef unsigned short    USHORT;
typedef unsigned int      UINT;
typedef long              LONG;
typedef unsigned long     ULONG;
typedef unsigned long*    PULONG;
typedef long long         LONG64;
typedef unsigned long long ULONG64;
typedef unsigned long long ULONGLONG;
typedef long long         LONGLONG;
typedef long              NTSTATUS;
typedef unsigned short    WORD;
typedef unsigned long     DWORD;
typedef int               BOOL;
typedef unsigned short    WCHAR;
typedef WCHAR*            PWSTR;
typedef const WCHAR*      PCWSTR;
typedef unsigned char     KIRQL;
typedef int8_t            INT8;
typedef uint8_t           UINT8;
typedef int16_t           INT16;
typedef uint16_t          UINT16;
typedef int32_t           INT32;
typedef uint32_t          UINT32;
typedef int64_t           INT64;
typedef uint64_t          UINT64;
typedef size_t            SIZE_T;
typedef size_t            ULONG_PTR;

#define IN
#define OUT
#define _In_
#define _In_opt_
#define _Inout_
#define _Inout_opt_
#define _Out_
#define _Out_opt_
#define _Outptr_
#define _COM_Outptr_
#define _IRQL_uses_cancel_
#define _In_reads_bytes_(x)
#define _Out_writes_bytes_(x)
#define _Out_writes_bytes_to_opt_(x, y)
#define PAGED_CODE() do { } while (0)
#define UNREFERENCED_PARAMETER(x) ((void)(x))
#define NTAPI
#define FORCEINLINE static inline
#define STDMETHODIMP NTSTATUS
#define STDMETHODIMP_(t) t

/* ---- statuses ---- */
#define STATUS_SUCCESS                 ((NTSTATUS)0)
#define STATUS_UNSUCCESSFUL            ((NTSTATUS)0xC0000001L)
#define STATUS_PENDING                 ((NTSTATUS)0x00000103L)
#define STATUS_INVALID_PARAMETER       ((NTSTATUS)0xC000000DL)
#define STATUS_INSUFFICIENT_RESOURCES  ((NTSTATUS)0xC000009AL)
#define STATUS_BUFFER_TOO_SMALL        ((NTSTATUS)0xC0000023L)
#define STATUS_BUFFER_OVERFLOW         ((NTSTATUS)0x80000005L)
#define STATUS_NOT_IMPLEMENTED         ((NTSTATUS)0xC0000002L)
#define STATUS_NOT_SUPPORTED           ((NTSTATUS)0xC00000BBL)
#define STATUS_NOT_FOUND               ((NTSTATUS)0xC0000225L)
#define STATUS_NO_MATCH                ((NTSTATUS)0xC0000272L)
#define STATUS_CANCELLED               ((NTSTATUS)0xC0000120L)
#define STATUS_INVALID_DEVICE_REQUEST  ((NTSTATUS)0xC0000010L)
#define STATUS_DEVICE_NOT_READY        ((NTSTATUS)0xC00000A3L)
#define STATUS_DEVICE_CONFIGURATION_ERROR ((NTSTATUS)0xC0000182L)
#define NT_SUCCESS(s) (((NTSTATUS)(s)) >= 0)

/* ---- IO/memory primitives ---- */
typedef struct _LIST_ENTRY { struct _LIST_ENTRY *Flink, *Blink; } LIST_ENTRY, *PLIST_ENTRY;
typedef struct _DEVICE_OBJECT  DEVICE_OBJECT, *PDEVICE_OBJECT;
typedef struct _IRP            IRP, *PIRP;
typedef struct _MDL            MDL, *PMDL;
typedef struct _DRIVER_OBJECT  DRIVER_OBJECT, *PDRIVER_OBJECT;
typedef struct _UNICODE_STRING { USHORT Length, MaximumLength; PWSTR Buffer; } UNICODE_STRING, *PUNICODE_STRING;
typedef struct _GUID { unsigned long Data1; unsigned short Data2,Data3; unsigned char Data4[8]; } GUID;
typedef GUID                 IID;
typedef const GUID&          REFGUID;
typedef const IID&           REFIID;
typedef GUID                 CLSID;
typedef const CLSID&         REFCLSID;
typedef struct _KSPIN_LOCK   { uintptr_t v; } KSPIN_LOCK;
typedef struct _KDPC         { void* p; } KDPC;
typedef struct _KTIMER       { void* p; } KTIMER;
typedef union _LARGE_INTEGER { struct { ULONG LowPart; LONG HighPart; }; LONGLONG QuadPart; } LARGE_INTEGER;

typedef enum _POOL_FLAGS { POOL_FLAG_NON_PAGED = 1, POOL_FLAG_PAGED = 2 } POOL_FLAGS;
typedef enum _MEMORY_CACHING_TYPE { MmCached = 0, MmNonCached, MmWriteCombined } MEMORY_CACHING_TYPE;
typedef enum _MM_PAGE_PRIORITY { LowPagePriority, NormalPagePriority = 16, HighPagePriority = 32 } MM_PAGE_PRIORITY;
#define MdlMappingNoExecute 0
typedef enum _INTERFACE_TYPE { PNPBus = 0 } INTERFACE_TYPE;
typedef NTSTATUS (NTAPI *PDRIVER_DISPATCH)(PDEVICE_OBJECT, PIRP);
typedef NTSTATUS (NTAPI *PDRIVER_ADD_DEVICE)(PDRIVER_OBJECT, PDEVICE_OBJECT);
typedef VOID     (NTAPI *PDRIVER_UNLOAD)(PDRIVER_OBJECT);
typedef NTSTATUS DRIVER_INITIALIZE(PDRIVER_OBJECT, PUNICODE_STRING);
typedef VOID DRIVER_CANCEL(PDEVICE_OBJECT, PIRP);
typedef VOID     (NTAPI *KDEFERRED_ROUTINE)(KDPC*, PVOID, PVOID, PVOID);

#define IRP_MJ_MAXIMUM_FUNCTION 0x1b
#define IRP_MJ_DEVICE_CONTROL    0x0e
#define IRP_MJ_PNP               0x1b
#define IRP_MJ_CREATE            0x00
#define IRP_MJ_CLOSE             0x02
#define IRP_MN_START_DEVICE      0x00
#define IRP_MN_REMOVE_DEVICE     0x02
#define IRP_MN_SURPRISE_REMOVAL  0x17
#define IO_NO_INCREMENT          0
#define IO_SOUND_INCREMENT       8
#define DO_DIRECT_IO             0x10
#define DO_DEVICE_INITIALIZING   0x80
#define METHOD_BUFFERED          0
#define METHOD_OUT_DIRECT        2
#define METHOD_IN_DIRECT         1
#define METHOD_NEITHER           3
#define FILE_READ_ACCESS         0x0001
#define FILE_WRITE_ACCESS        0x0002
#define FILE_DEVICE_UNKNOWN      0x00000022
#define CTL_CODE(devtype, fn, mthd, acc) \
    (((devtype) << 16) | ((acc) << 14) | ((fn) << 2) | (mthd))

typedef struct _DEVICE_DESCRIPTION {
    ULONG Master, ScatterGather, Dma32BitAddresses, Dma64BitAddresses;
    INTERFACE_TYPE InterfaceType;
    ULONG MaximumLength;
} DEVICE_DESCRIPTION, *PDEVICE_DESCRIPTION;

typedef struct _IO_STATUS_BLOCK { NTSTATUS Status; ULONG_PTR Information; } IO_STATUS_BLOCK;

struct _IRP {
    IO_STATUS_BLOCK IoStatus;
    PMDL MdlAddress;
    KIRQL CancelIrql;
    BOOLEAN Cancel;
    struct { PVOID SystemBuffer; } AssociatedIrp;
    struct {
        struct { LIST_ENTRY ListEntry; } Overlay;
    } Tail;
};

struct _DEVICE_OBJECT {
    ULONG Flags;
    PVOID DeviceExtension;
    PDEVICE_OBJECT NextDevice;
};

struct _DRIVER_EXTENSION { PDRIVER_ADD_DEVICE AddDevice; };
struct _DRIVER_OBJECT {
    PDEVICE_OBJECT DeviceObject;
    PDRIVER_DISPATCH MajorFunction[IRP_MJ_MAXIMUM_FUNCTION + 1];
    PDRIVER_UNLOAD DriverUnload;
    struct _DRIVER_EXTENSION* DriverExtension;
};

typedef struct _IO_STACK_LOCATION {
    UCHAR MajorFunction;
    UCHAR MinorFunction;
    struct {
        struct {
            ULONG OutputBufferLength;
            ULONG InputBufferLength;
            ULONG IoControlCode;
        } DeviceIoControl;
    } Parameters;
} IO_STACK_LOCATION, *PIO_STACK_LOCATION;

static inline PIO_STACK_LOCATION IoGetCurrentIrpStackLocation(PIRP) { static IO_STACK_LOCATION s; return &s; }
static inline VOID IoCompleteRequest(PIRP, int) {}
static inline VOID IoMarkIrpPending(PIRP) {}
static inline DRIVER_CANCEL* IoSetCancelRoutine(PIRP, DRIVER_CANCEL* r) { (void)r; return 0; }
static inline VOID IoReleaseCancelSpinLock(KIRQL) {}
static inline NTSTATUS IoAllocateMdl(PVOID, ULONG, BOOLEAN, BOOLEAN, PIRP) { return STATUS_SUCCESS; }
static inline VOID MmBuildMdlForNonPagedPool(PMDL) {}
static inline PVOID MmGetSystemAddressForMdlSafe(PMDL, int) { return nullptr; }
static inline VOID IoFreeMdl(PMDL) {}
static inline NTSTATUS IoRegisterDeviceInterface(PDEVICE_OBJECT, const GUID*, PUNICODE_STRING, PUNICODE_STRING) { return 0; }
static inline VOID IoSetDeviceInterfaceState(PUNICODE_STRING, BOOLEAN) {}
static inline NTSTATUS IoCreateSymbolicLink(PUNICODE_STRING, PUNICODE_STRING) { return 0; }
static inline VOID IoDeleteSymbolicLink(PUNICODE_STRING) {}
static inline VOID RtlInitUnicodeString(PUNICODE_STRING s, PCWSTR x) { (void)s; (void)x; }
static inline VOID RtlFreeUnicodeString(PUNICODE_STRING) {}
static inline VOID InitializeListHead(PLIST_ENTRY e) { e->Flink = e->Blink = e; }
static inline BOOLEAN IsListEmpty(PLIST_ENTRY e) { return e->Flink == e; }
static inline VOID InsertHeadList(PLIST_ENTRY h, PLIST_ENTRY e) { e->Flink = h->Flink; e->Blink = h; h->Flink->Blink = e; h->Flink = e; }
static inline VOID InsertTailList(PLIST_ENTRY h, PLIST_ENTRY e) { InsertHeadList(h->Blink, e); }
static inline PLIST_ENTRY RemoveHeadList(PLIST_ENTRY h) { PLIST_ENTRY e = h->Flink; e->Flink->Blink = h; h->Flink = e->Flink; return e; }
static inline VOID RemoveEntryList(PLIST_ENTRY e) { e->Flink->Blink = e->Blink; e->Blink->Flink = e->Flink; e->Flink = e->Blink = e; }

#define CONTAINING_RECORD(addr, type, field) \
    ((type*)((char*)(addr) - offsetof(type, field)))

static inline VOID KeInitializeSpinLock(KSPIN_LOCK* s) { s->v = 0; }
static inline VOID KeAcquireSpinLock(KSPIN_LOCK*, KIRQL* o) { *o = 0; }
static inline VOID KeReleaseSpinLock(KSPIN_LOCK*, KIRQL) {}
static inline VOID KeAcquireSpinLockAtDpcLevel(KSPIN_LOCK*) {}
static inline VOID KeReleaseSpinLockFromDpcLevel(KSPIN_LOCK*) {}
static inline VOID KeInitializeTimer(KTIMER*) {}
static inline VOID KeInitializeDpc(KDPC*, KDEFERRED_ROUTINE, PVOID) {}
static inline BOOLEAN KeSetTimerEx(KTIMER*, LARGE_INTEGER, LONG, KDPC*) { return 0; }
static inline BOOLEAN KeCancelTimer(KTIMER*) { return 0; }
static inline VOID KeFlushQueuedDpcs() {}
static inline LARGE_INTEGER KeQueryPerformanceCounter(LARGE_INTEGER* freq) {
    LARGE_INTEGER x; x.QuadPart = 0; if (freq) { freq->QuadPart = 10000000; } return x;
}

static inline LONG InterlockedExchange(volatile LONG* t, LONG v) { LONG old = *t; *t = v; return old; }
static inline LONG64 ReadAcquire64(volatile LONG64* p) { return *p; }

static inline PVOID ExAllocatePool2(POOL_FLAGS, SIZE_T n, ULONG) { return ::malloc(n); }
static inline VOID  ExFreePoolWithTag(PVOID p, ULONG) { ::free(p); }

#define RtlZeroMemory(p,n) memset((p),0,(n))
#define RtlCopyMemory(d,s,n) memcpy((d),(s),(n))

#define DbgPrintEx(...) ((void)0)
#define DPFLTR_IHVAUDIO_ID 0
#define DPFLTR_INFO_LEVEL  3

#define DEFINE_GUID(name, ...) static const GUID name = {0,0,0,{0,0,0,0,0,0,0,0}}


/* ---- PortCls/KS minimal stubs (just enough for driver.h to parse). ---- */
class IUnknown;
typedef IUnknown* PUNKNOWN;
typedef struct _RESOURCELIST* PRESOURCELIST;
