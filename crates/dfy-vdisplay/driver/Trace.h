/* SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0 */
/* Copyright (c) 2025 Dafeiyu Remote Project */

/*
 * Trace.h - WPP (Windows software trace preprocessor) control GUID and flags.
 *
 * driver.c includes "driver.tmh" AFTER driver.h; the WDK WPP preprocessor generates
 * driver.tmh from the trace macros declared at the bottom of this file.  Trace.tmh is
 * build output and must not be committed.
 *
 * Bring-up: build with /p:RunWpp=true (WDK property "Trace"), enable the provider with
 *   tracelog -start dfy -guid <guid> -flag 0xF -level 5 -rt -f dfy.etl
 * or use traceview/tracefmt to read the ETL.  Field debugging should prefer this over
 * DbgPrint: a display driver must not print in the frame path.
 */

#pragma once

/* {5B2E5C8B-4A8C-4E5A-9C55-7A0F9E1B2C3D} - our own GUID, do not reuse the sample's. */
#define WPP_CONTROL_GUIDS                                                    \
    WPP_DEFINE_CONTROL_GUID(                                                 \
        DfyDisplayTraceGuid,                                                \
        (5B2E5C8B, 4A8C, 4E5A, 9C, 55, 7A, 0F, 9E, 1B, 2C, 3D),             \
        WPP_DEFINE_BIT(TRACE_DRIVER)                                        \
        WPP_DEFINE_BIT(TRACE_CONTROL)                                       \
        WPP_DEFINE_BIT(TRACE_MONITOR)                                       \
        WPP_DEFINE_BIT(TRACE_SWAPCHAIN))

#define WPP_FLAG_LEVEL_LOGGER(flag, level) WPP_LEVEL_LOGGER(flag)
#define WPP_FLAG_LEVEL_ENABLED(flag, level) \
    (WPP_LEVEL_ENABLED(flag) && WPP_CONTROL(WPP_BIT_##flag).Level >= level)

#define WPP_LEVEL_FLAGS_LOGGER(lvl, flags) WPP_LEVEL_LOGGER(flags)
#define WPP_LEVEL_FLAGS_ENABLED(lvl, flags) \
    (WPP_LEVEL_ENABLED(flags) && WPP_CONTROL(WPP_BIT_##flags).Level >= lvl)

/* begin_wpp config
FUNC TraceEvents(LEVEL, FLAGS, MSG, ...);
USEPREFIX(TraceEvents, "%!STDPREFIX!");
end_wpp */