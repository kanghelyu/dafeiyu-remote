@echo off
rem SPDX-License-Identifier: AGPL-3.0-or-later
rem Copyright (c) 2025 Dafeiyu Remote Project
rem
rem build.bat - WDK build driver for the Dafeiyu virtual display driver.
rem
rem This script is documentation-as-code: it records the exact commands used to build
rem and sign the driver package.  The vcxproj is intentionally NOT in this skeleton (see
rem README section 5) - generate it once from the WDK "WDM Driver" template, keep
rem TargetName=dfy-vdisplay, DriverType=UMDF, ConfigurationType=DynamicLibrary, and then
rem this script works without changes.
rem
rem Usage:  build.bat [x64|arm64] [Release|Debug]
rem Requires: Visual Studio 2022 + WDK 10.0.26100 (or newer) in a normal elevated shell.

setlocal enabledelayedexpansion

set ARCH=%~1
if "%ARCH%"=="" set ARCH=x64
set CONFIG=%~2
if "%CONFIG%"=="" set CONFIG=Release

rem --- Locate the WDK.  Adjust WDK_VERSION if a newer WDK is installed. -------------
if "%WDK_VERSION%"=="" set WDK_VERSION=10.0.26100.0
set WDK_ROOT=%ProgramFiles(x86)%\Windows Kits\10
set WDK_BIN=%WDK_ROOT%\bin\%WDK_VERSION%
if not exist "%WDK_BIN%" (
    echo [error] WDK %WDK_VERSION% not found under "%WDK_ROOT%".
    echo         Install "Windows Driver Kit" from the Visual Studio installer, or set
    echo         WDK_VERSION to the installed version.
    exit /b 1
)

set STAMPINF=%WDK_BIN%\%ARCH%\stampinf.exe
set INF2CAT=%WDK_BIN%\x86\Inf2Cat.exe

rem --- MSBuild: prefer the VS installation's driver targets. -----------------------
where msbuild >nul 2>nul
if errorlevel 1 (
    echo [error] msbuild.exe not on PATH. Run this from a "x64 Native Tools" prompt or
    echo         add the Visual Studio MSBuild directory to PATH.
    exit /b 1
)

echo === 1/4 compiling the UMDF driver DLL (%ARCH%/%CONFIG%) ===
if not exist dfy-vdisplay.vcxproj (
    echo [todo] dfy-vdisplay.vcxproj is missing.
    echo        Generate it from the WDK "WDM Driver" project template, then re-run.
    exit /b 2
)
msbuild dfy-vdisplay.vcxproj /p:Configuration=%CONFIG% /p:Platform=%ARCH% /p:WDKVersion=%WDK_VERSION% /m /v:minimal
if errorlevel 1 exit /b 1

echo === 2/4 stamping DriverVer into the INF ===
rem CI should override the version:  set DFY_DRIVER_VERSION=0.1.0.%BUILD_NUMBER%
if "%DFY_DRIVER_VERSION%"=="" set DFY_DRIVER_VERSION=0.1.0.0
"%STAMPINF%" -f dfy-vdisplay.inf -d * -v %DFY_DRIVER_VERSION%
if errorlevel 1 exit /b 1

echo === 3/4 building the catalog ===
"%INF2CAT%" /driver:. /os:10_X64,10_ARM64
if errorlevel 1 (
    echo [error] Inf2Cat failed - a driver package cannot be installed without a valid .cat
    exit /b 1
)

echo === 4/4 signing ===
rem Dev signing (test certificate).  For the WHQL-signed package this step is replaced by
rem the Microsoft-signed catalog returned from Partner Center; see README section 7.
if "%DFY_CERT_PFX%"=="" (
    echo [skip] DFY_CERT_PFX not set - catalog and DLL left unsigned.
    echo        For a local test signature:
    echo          set DFY_CERT_PFX=^<path^> && set DFY_CERT_PASSWORD=^<pw^>
    echo        Requires ^"bcdedit /set testsigning on^" + reboot on the test machine.
) else (
    signtool sign /fd sha256 /f "%DFY_CERT_PFX%" /p "%DFY_CERT_PASSWORD%" ^
        /tr http://timestamp.digicert.com /td sha256 /v dfy-vdisplay.cat
    signtool sign /fd sha256 /f "%DFY_CERT_PFX%" /p "%DFY_CERT_PASSWORD%" ^
        /tr http://timestamp.digicert.com /td sha256 /v %CONFIG%\%ARCH%\dfy-vdisplay.dll
)

echo.
echo Done. Install on a test machine with:
echo   pnputil /add-driver dfy-vdisplay.inf /install
echo and verify with:
echo   Get-PnpDevice -FriendlyName "*Dafeiyu*"
endlocal