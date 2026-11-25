/* SPDX-License-Identifier: AGPL-3.0-or-later */
/* Copyright (c) 2025 Dafeiyu Remote Project */

/*
 * driver.h - shared declarations for the Dafeiyu virtual display driver
 *            (UMDF 2 indirect display driver, IddCx 1.2).
 *
 * This header is the compile-time contract between the driver and the user-mode
 * agent (crates/dfy-vdisplay/src/win_driver.rs).  Every struct here is submitted to
 * static asserts at the bottom of the file; the Rust side asserts the same sizes.
 * Change one side only after bumping DFY_VDISPLAY_PROTOCOL_VERSION on both sides.
 *
 * NOTE ON SIGNATURES: the EvtIddCx* prototypes below follow the IddCx 1.2 callback
 * typedefs as used by Microsoft's IddSampleDriver (which we read, MIT, no code
 * copied).  Some typedefs grew parameters in later WDKs; on the first real build
 * verify each one against WDK 10.0.26100 iddcx.h and adapt.  Nothing here has been
 * compiled against a WDK yet - see ../README.md.
 */

#pragma once

#include <windows.h>
#include <wdf.h>
#include <iddcx.h>
#include <d3d11.h>
#include <dxgi1_5.h>
#include <objbase.h>

#include "Trace.h"

/* ------------------------------------------------------------------------- */
/* Build-time tunables                                                       */
/* ------------------------------------------------------------------------- */

#define DFY_VDISPLAY_MAX_MONITORS       8       /* keep in sync with Rust MAX_VIRTUAL_DISPLAYS */
#define DFY_VDISPLAY_NAME_MAX           64      /* name_hint payload size, incl. NUL */
#define DFY_VDISPLAY_MAX_MODES          32      /* modes advertised per connector */
#define DFY_VDISPLAY_MAX_PENDING_EVENTS 64      /* driver->agent event ring */

#define DFY_VDISPLAY_PROTOCOL_VERSION   1
#define DFY_VDISPLAY_CAP_MULTI_MONITOR  (1u << 0)
#define DFY_VDISPLAY_CAP_DYNAMIC_MODES  (1u << 1)
#define DFY_VDISPLAY_CAP_HOTPLUG_EVENTS (1u << 2)

/* Keep our display id space distinct from "no display": the Rust side filters this. */
#define DFY_VDISPLAY_INVALID_ID         0xFFFFFFFFu

/* ------------------------------------------------------------------------- */
/* Device interface + control plane (must match src/win_driver.rs)            */
/* ------------------------------------------------------------------------- */

/* {54EC77FA-1377-44E6-8C32-88FD5F44C84C}, the indirect-display device interface class */
#define DFY_VDISPLAY_DEVICE_INTERFACE_GUID                  \
    { 0x54ec77fa, 0x1377, 0x44e6,                           \
      { 0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c } }

/*
 * Auto-reset named event signalled on every monitor arrival/departure and every
 * mode-table change.  The agent waits on it in WinDriverBackend::wait_for_change and
 * then drains events with IOCTL_VDISPLAY_TAKE_EVENT.  The event name is global to
 * the session (no namespace prefix) because the agent runs as a normal user and the
 * driver runs inside WUDFHost in session 0 for a root-enumerated device.
 */
#define DFY_VDISPLAY_EVENT_NAME         L"DafeiyuVDisplayDeviceEvent"

/* CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800.., METHOD_BUFFERED, FILE_ANY_ACCESS) */
#define DFY_VDISPLAY_CTL_BASE           0x800u
#define IOCTL_VDISPLAY_CREATE           \
    CTL_CODE(FILE_DEVICE_UNKNOWN, DFY_VDISPLAY_CTL_BASE + 0, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_VDISPLAY_UPDATE           \
    CTL_CODE(FILE_DEVICE_UNKNOWN, DFY_VDISPLAY_CTL_BASE + 1, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_VDISPLAY_DESTROY          \
    CTL_CODE(FILE_DEVICE_UNKNOWN, DFY_VDISPLAY_CTL_BASE + 2, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_VDISPLAY_LIST             \
    CTL_CODE(FILE_DEVICE_UNKNOWN, DFY_VDISPLAY_CTL_BASE + 3, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_VDISPLAY_TAKE_EVENT       \
    CTL_CODE(FILE_DEVICE_UNKNOWN, DFY_VDISPLAY_CTL_BASE + 4, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_VDISPLAY_GET_PROTOCOL_INFO \
    CTL_CODE(FILE_DEVICE_UNKNOWN, DFY_VDISPLAY_CTL_BASE + 5, METHOD_BUFFERED, FILE_ANY_ACCESS)

/* Event record kinds (Rust: win_driver::event_kind) */
#define DFY_EVENT_MONITOR_ARRIVAL       1u
#define DFY_EVENT_MONITOR_DEPARTURE     2u
#define DFY_EVENT_MODES_CHANGED         3u

#pragma pack(push, 4)

/* IOCTL_VDISPLAY_CREATE input. 80 bytes. */
typedef struct _DFY_VDISPLAY_CREATE {
    UINT32 Width;
    UINT32 Height;
    UINT32 RefreshHz;
    UINT32 Dpi;
    UINT8  NameHint[DFY_VDISPLAY_NAME_MAX]; /* NUL-terminated UTF-8, may be empty */
} DFY_VDISPLAY_CREATE, *PDFY_VDISPLAY_CREATE;

/* IOCTL_VDISPLAY_UPDATE input. 20 bytes. id = connector index chosen by CREATE. */
typedef struct _DFY_VDISPLAY_UPDATE {
    UINT32 Id;
    UINT32 Width;
    UINT32 Height;
    UINT32 RefreshHz;
    UINT32 Dpi;
} DFY_VDISPLAY_UPDATE, *PDFY_VDISPLAY_UPDATE;

/* IOCTL_VDISPLAY_DESTROY input. 4 bytes. */
typedef struct _DFY_VDISPLAY_DESTROY {
    UINT32 Id;
} DFY_VDISPLAY_DESTROY, *PDFY_VDISPLAY_DESTROY;

/* One record from IOCTL_VDISPLAY_TAKE_EVENT. 8 bytes. */
typedef struct _DFY_VDISPLAY_EVENT {
    UINT32 Kind; /* DFY_EVENT_* */
    UINT32 Id;
} DFY_VDISPLAY_EVENT, *PDFY_VDISPLAY_EVENT;

/* IOCTL_VDISPLAY_LIST output header. 8 bytes. */
typedef struct _DFY_VDISPLAY_STATE_LIST_HEADER {
    UINT32 Count;
    UINT32 Reserved;
} DFY_VDISPLAY_STATE_LIST_HEADER;

/* One display state. 24 bytes. 0 = detached, 1 = attached (monitor arrived). */
typedef struct _DFY_VDISPLAY_STATE {
    UINT32 Id;
    UINT32 Width;
    UINT32 Height;
    UINT32 RefreshHz;
    UINT32 Dpi;
    UINT32 Attached;
} DFY_VDISPLAY_STATE, *PDFY_VDISPLAY_STATE;

/* IOCTL_VDISPLAY_LIST output. 8 + 24*8 = 200 bytes. */
typedef struct _DFY_VDISPLAY_STATE_LIST {
    DFY_VDISPLAY_STATE_LIST_HEADER Header;
    DFY_VDISPLAY_STATE States[DFY_VDISPLAY_MAX_MONITORS];
} DFY_VDISPLAY_STATE_LIST, *PDFY_VDISPLAY_STATE_LIST;

/* IOCTL_VDISPLAY_GET_PROTOCOL_INFO output. 8 bytes. */
typedef struct _DFY_VDISPLAY_PROTOCOL_INFO {
    UINT32 ProtocolVersion;
    UINT32 Capabilities; /* DFY_VDISPLAY_CAP_* bitmask */
} DFY_VDISPLAY_PROTOCOL_INFO, *PDFY_VDISPLAY_PROTOCOL_INFO;

#pragma pack(pop)

C_ASSERT(sizeof(DFY_VDISPLAY_CREATE) == 80);
C_ASSERT(sizeof(DFY_VDISPLAY_UPDATE) == 20);
C_ASSERT(sizeof(DFY_VDISPLAY_DESTROY) == 4);
C_ASSERT(sizeof(DFY_VDISPLAY_EVENT) == 8);
C_ASSERT(sizeof(DFY_VDISPLAY_STATE_LIST) == 200);
C_ASSERT(sizeof(DFY_VDISPLAY_PROTOCOL_INFO) == 8);

/* ------------------------------------------------------------------------- */
/* Driver-private types                                                      */
/* ------------------------------------------------------------------------- */

/* A mode in the per-connector table the driver owns and the agent mutates. */
typedef struct _DFY_MODE {
    UINT32 Width;
    UINT32 Height;
    UINT32 RefreshHz;
    UINT32 Dpi;
} DFY_MODE;

/*
 * Per-monitor swap-chain worker.  One thread per assigned swap chain; it owns the
 * D3D11 device created for the render adapter LUID and the IddCx swap-chain handle.
 * Lifetime rule: the thread is the *only* user of SwapChain and it deletes the
 * handle itself when it exits (after UnassignSwapChain has signalled Terminate).
 */
typedef struct _DFY_SWAPCHAIN_WORKER {
    HANDLE          Thread;
    HANDLE          TerminateEvent; /* signalled by DfyEvtIddCxMonitorUnassignSwapChain */
    IDDCX_SWAPCHAIN SwapChain;
    HANDLE          NewFrameEvent;  /* from IDARG_IN_SETSWAPCHAIN */
    LUID            RenderAdapterLuid;
    UINT32          DisplayId;
} DFY_SWAPCHAIN_WORKER, *PDFY_SWAPCHAIN_WORKER;

/*
 * State of one virtual display; the source of truth for one ConnectorIndex slot.
 * Kept in the device context and looked up by IDDCX_MONITOR handle: attaching a WDF
 * context to the monitor object through IddCx requires knowledge of which WDK version
 * exposes monitor attributes in IDARG_IN_MONITORCREATE, so phase 1 uses this table
 * (see README §3).  Switching to a WDF context later is a local change.
 */
typedef struct _DFY_DISPLAY_SLOT {
    BOOLEAN              Used;     /* slot claimed by IOCTL_VDISPLAY_CREATE */
    BOOLEAN              Arrived;  /* IddCxMonitorArrival called and not withdrawn */
    UINT32               DisplayId;/* == ConnectorIndex, also returned to the agent */
    IDDCX_MONITOR        Monitor;  /* NULL while !Arrived */
    DFY_MODE             Current;  /* most recently requested mode */
    UINT32               ModeCount;/* modes advertised for this slot */
    DFY_MODE             Modes[DFY_VDISPLAY_MAX_MODES];
    UINT8                NameHint[DFY_VDISPLAY_NAME_MAX];
    UINT8                Edid[128];/* EDID 1.3 base block we synthesize (identity + preferred mode) */
    UINT32               EdidSize;
    DFY_SWAPCHAIN_WORKER Worker;   /* zeroed when no swap chain is assigned */
} DFY_DISPLAY_SLOT;

/* Per-device context. */
typedef struct _DFY_DEVICE_CONTEXT {
    WDFDEVICE       WdfDevice;
    IDDCX_ADAPTER   Adapter;
    BOOLEAN         AdapterReady;         /* set in EvtIddCxAdapterInitFinished */
    HANDLE          DeviceEvent;          /* DFY_VDISPLAY_EVENT_NAME, auto-reset */
    WDFWAITLOCK     StateLock;            /* protects Slots + event ring (PASSIVE_LEVEL) */
    DFY_DISPLAY_SLOT Slots[DFY_VDISPLAY_MAX_MONITORS];
    /* Driver -> agent event ring. Producers: monitor arrival/departure, UPDATE. */
    DFY_VDISPLAY_EVENT EventRing[DFY_VDISPLAY_MAX_PENDING_EVENTS];
    UINT32          EventHead;            /* next slot to write */
    UINT32          EventCount;           /* valid records */
} DFY_DEVICE_CONTEXT, *PDFY_DEVICE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DFY_DEVICE_CONTEXT, DfyGetDeviceContext)

/*
 * Context attached to the IDDCX_ADAPTER object (created through
 * IDARG_IN_ADAPTER_INIT.ObjectAttributes in DfyInitAdapter).  It exists for one reason:
 * EvtIddCxAdapterInitFinished and the swap-chain callbacks are handed an IddCx object
 * handle, not a WDFDEVICE, so this is the path back to our device state.
 */
typedef struct _DFY_ADAPTER_CONTEXT {
    PDFY_DEVICE_CONTEXT DeviceContext;
} DFY_ADAPTER_CONTEXT, *PDFY_ADAPTER_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DFY_ADAPTER_CONTEXT, DfyGetAdapterContext)

/*
 * Phase 1 assumes a single virtual display adapter per driver instance (the INF
 * installs exactly one root device).  This accessor returns the context of the last
 * device added; it is set in DfyEvtDeviceAdd and cleared in DfyEvtDeviceCleanup.
 * TODO(phase-2): replace with a monitor-object WDF context once multiple adapters per
 * process are supported.
 */
PDFY_DEVICE_CONTEXT DfyCurrentDeviceContext(void);

/* ------------------------------------------------------------------------- */
/* Entry points                                                              */
/* ------------------------------------------------------------------------- */

DRIVER_INITIALIZE DriverEntry;

EVT_WDF_DRIVER_DEVICE_ADD               DfyEvtDeviceAdd;
EVT_WDF_DEVICE_D0_ENTRY                 DfyEvtDeviceD0Entry;
EVT_WDF_DEVICE_D0_EXIT                  DfyEvtDeviceD0Exit;
EVT_WDF_OBJECT_CONTEXT_CLEANUP          DfyEvtDeviceCleanup;

/*
 * IddCx callbacks.  Prototypes follow IddCx 1.2 as used by the IddCx sample;
 * re-verify against iddcx.h on first build (typedefs changed after 1.2).
 */
NTSTATUS DfyEvtIddCxDeviceIoControl(
    WDFDEVICE Device,
    WDFREQUEST Request,
    size_t OutputBufferLength,
    size_t InputBufferLength,
    ULONG IoControlCode);

NTSTATUS DfyEvtIddCxAdapterInitFinished(
    IDDCX_ADAPTER AdapterObject,
    const IDARG_IN_ADAPTER_INIT_FINISHED *pInArgs);

NTSTATUS DfyEvtIddCxParseMonitorDescription(
    const IDARG_IN_PARSEMONITORDESCRIPTION *pInArgs,
    IDARG_OUT_PARSEMONITORDESCRIPTION *pOutArgs);

NTSTATUS DfyEvtIddCxMonitorGetDefaultDescriptionModes(
    IDDCX_MONITOR MonitorObject,
    const IDARG_IN_GETDEFAULTDESCRIPTIONMODES *pInArgs,
    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES *pOutArgs);

NTSTATUS DfyEvtIddCxMonitorQueryTargetModes(
    IDDCX_MONITOR MonitorObject,
    const IDARG_IN_QUERYTARGETMODES *pInArgs,
    IDARG_OUT_QUERYTARGETMODES *pOutArgs);

NTSTATUS DfyEvtIddCxAdapterCommitModes(
    IDDCX_ADAPTER AdapterObject,
    const IDARG_IN_COMMITMODES *pInArgs);

NTSTATUS DfyEvtIddCxMonitorAssignSwapChain(
    IDDCX_MONITOR MonitorObject,
    const IDARG_IN_SETSWAPCHAIN *pInArgs);

VOID DfyEvtIddCxMonitorUnassignSwapChain(IDDCX_MONITOR MonitorObject);

/* ------------------------------------------------------------------------- */
/* Internal helpers (driver.c)                                               */
/* ------------------------------------------------------------------------- */

NTSTATUS DfyInitAdapter(PDFY_DEVICE_CONTEXT DeviceContext);
NTSTATUS DfyCreateMonitor(PDFY_DEVICE_CONTEXT DeviceContext, UINT32 ConnectorIndex);
void     DfyDestroyMonitor(PDFY_DEVICE_CONTEXT DeviceContext, UINT32 ConnectorIndex);

/* Slot lookup; both return NULL when not found. Call with StateLock held. */
PDFY_DISPLAY_SLOT DfyFindSlotById(PDFY_DEVICE_CONTEXT DeviceContext, UINT32 DisplayId);
PDFY_DISPLAY_SLOT DfyFindSlotByMonitor(PDFY_DEVICE_CONTEXT DeviceContext, IDDCX_MONITOR Monitor);

/* Mode-table conversion + EDID synthesis. */
void     DfyBuildModeTable(PDFY_DISPLAY_SLOT Slot);
void     DfyBuildEdid(const DFY_MODE *Mode, const UINT8 *NameHint, UINT8 *Edid, UINT32 *EdidSize);
IDDCX_TARGET_MODE DfyMakeTargetMode(const DFY_MODE *Mode);

/* Event ring helpers: called with StateLock held. */
void     DfyPushEventLocked(PDFY_DEVICE_CONTEXT DeviceContext, UINT32 Kind, UINT32 Id);

/* Swap-chain worker. */
NTSTATUS DfyStartSwapChainWorker(PDFY_DEVICE_CONTEXT DeviceContext, PDFY_DISPLAY_SLOT Slot,
                                 const IDARG_IN_SETSWAPCHAIN *pInArgs);
void     DfyStopSwapChainWorker(PDFY_DISPLAY_SLOT Slot);
DWORD    WINAPI DfySwapChainThreadProc(LPVOID Parameter);

/* IOCTL handlers (each completes the WDFREQUEST, including on error). */
VOID DfyHandleIoctlCreate(PDFY_DEVICE_CONTEXT DeviceContext, WDFREQUEST Request,
                          size_t InputLength, size_t OutputLength);
VOID DfyHandleIoctlUpdate(PDFY_DEVICE_CONTEXT DeviceContext, WDFREQUEST Request,
                          size_t InputLength);
VOID DfyHandleIoctlDestroy(PDFY_DEVICE_CONTEXT DeviceContext, WDFREQUEST Request,
                           size_t InputLength);
VOID DfyHandleIoctlList(PDFY_DEVICE_CONTEXT DeviceContext, WDFREQUEST Request,
                        size_t OutputLength);
VOID DfyHandleIoctlTakeEvent(PDFY_DEVICE_CONTEXT DeviceContext, WDFREQUEST Request,
                             size_t OutputLength);
VOID DfyHandleIoctlGetProtocolInfo(PDFY_DEVICE_CONTEXT DeviceContext, WDFREQUEST Request,
                                   size_t OutputLength);

/* Complete a request with the given status and no output bytes. */
VOID DfyCompleteRequest(WDFREQUEST Request, NTSTATUS Status, size_t Information);

/* Validate a mode against the ranges the agent also enforces. */
NTSTATUS DfyValidateMode(UINT32 Width, UINT32 Height, UINT32 RefreshHz, UINT32 Dpi);