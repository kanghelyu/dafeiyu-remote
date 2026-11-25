# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (c) 2025 Dafeiyu Remote Project
#
# Makefile - nmake wrapper around the WDK/MSBuild flow described in build.bat.
#
# The classic WDK "sources/BUILD" makefiles are deprecated; MSBuild projects are the
# supported path, so this Makefile only calls MSBuild and the SDK tools.  It exists for
# developers and CI that prefer `nmake /f Makefile` over a .bat file.
#
#   nmake /f Makefile            # build + catalog
#   nmake /f Makefile sign       # sign with DFY_CERT_PFX
#   nmake /f Makefile install    # pnputil install (elevated)
#   nmake /f Makefile uninstall  # pnputil uninstall (elevated)
#   nmake /f Makefile clean
#
# Run from a "x64 Native Tools Command Prompt for VS 2022" with the WDK installed.

ARCH        = x64
CONFIG      = Release
WDK_VERSION = 10.0.26100.0
WDK_BIN     = $(ProgramFiles(x86))\Windows Kits\10\bin\$(WDK_VERSION)
STAMPINF    = "$(WDK_BIN)\$(ARCH)\stampinf.exe"
INF2CAT     = "$(WDK_BIN)\x86\Inf2Cat.exe"

# Override in CI: nmake /f Makefile DFY_DRIVER_VERSION=0.1.0.42
DFY_DRIVER_VERSION = 0.1.0.0

PROJECT     = dfy-vdisplay.vcxproj
INF         = dfy-vdisplay.inf
CAT         = dfy-vdisplay.cat
DLL         = $(CONFIG)\$(ARCH)\dfy-vdisplay.dll

all: build inf cat

# The vcxproj is not committed in this skeleton - see README section 5.
build:
	@if not exist "$(PROJECT)" ( echo [todo] $(PROJECT) is missing: generate it from the WDK WDM Driver template & exit /b 2 )
	msbuild "$(PROJECT)" /p:Configuration=$(CONFIG) /p:Platform=$(ARCH) /p:WDKVersion=$(WDK_VERSION) /m /v:minimal

inf:
	$(STAMPINF) -f $(INF) -d * -v $(DFY_DRIVER_VERSION)

cat: inf
	$(INF2CAT) /driver:. /os:10_X64,10_ARM64

sign: build cat
	@if not defined DFY_CERT_PFX ( echo [todo] set DFY_CERT_PFX and DFY_CERT_PASSWORD, see README section 7 & exit /b 2 )
	signtool sign /fd sha256 /f "$(DFY_CERT_PFX)" /p "$(DFY_CERT_PASSWORD)" /tr http://timestamp.digicert.com /td sha256 /v $(CAT)
	signtool sign /fd sha256 /f "$(DFY_CERT_PFX)" /p "$(DFY_CERT_PASSWORD)" /tr http://timestamp.digicert.com /td sha256 /v $(DLL)

install: cat
	pnputil /add-driver $(INF) /install

uninstall:
	pnputil /delete-driver $(INF) /uninstall /force

clean:
	-@if exist "$(CONFIG)" rmdir /s /q "$(CONFIG)"
	-@if exist "$(CAT)" del /q "$(CAT)"
	-@if exist "driver.tmh" del /q "driver.tmh"

help:
	@echo Targets: all build inf cat sign install uninstall clean
	@echo Variables: ARCH=$(ARCH) CONFIG=$(CONFIG) WDK_VERSION=$(WDK_VERSION)