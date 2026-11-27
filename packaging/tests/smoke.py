#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
"""Host-independent packaging validation plus real archive/package smoke tests."""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
import plistlib
import re
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[2]
PACKAGING = ROOT / "packaging"


def run(*args: object, cwd: Path = ROOT, env: dict[str, str] | None = None, ok: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run([str(item) for item in args], cwd=cwd, env=env, text=True, capture_output=True)
    if ok and result.returncode:
        raise AssertionError(f"command failed ({result.returncode}): {args}\n{result.stdout}\n{result.stderr}")
    if not ok and not result.returncode:
        raise AssertionError(f"command unexpectedly succeeded: {args}")
    return result


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_syntax() -> None:
    scripts = sorted((ROOT / "deploy").rglob("*.sh")) + sorted(PACKAGING.rglob("*.sh"))
    scripts.append(PACKAGING / "linux/debian/postinst")
    for script in scripts:
        run("bash", "-n", script)
    for source in sorted(PACKAGING.rglob("*.py")):
        compile(source.read_text(encoding="utf-8"), str(source), "exec")
    print("PASS every scoped shell script and Python source parses")


def check_workflow() -> None:
    try:
        import yaml
    except ImportError as error:
        raise AssertionError("PyYAML is required for workflow validation") from error
    workflow = yaml.load((ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8"), Loader=yaml.BaseLoader)
    assert "push" in workflow["on"] and "workflow_dispatch" in workflow["on"]
    jobs = workflow["jobs"]
    assert {"build", "windows-installer", "linux-packages", "macos-installers", "android", "publish"} <= jobs.keys()
    assert set(jobs["publish"]["needs"]) == {"build", "windows-installer", "linux-packages", "macos-installers", "android"}
    targets = {item["target"] for item in jobs["build"]["strategy"]["matrix"]["include"]}
    assert targets == {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin"}
    text = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
    assert "dfy-relay-linux-${arch}.tar.gz" in (ROOT / "deploy/install-relay.sh").read_text(encoding="utf-8")
    assert "sha256sum * > SHA256SUMS.txt" in text and "|| true" not in text
    assert "ANDROID_KEYSTORE_BASE64" in text and "MAC_SIGNING_IDENTITY" in text
    assert workflow["permissions"]["contents"] == "read"
    assert jobs["publish"]["permissions"]["contents"] == "write"
    assert "refs/tags/${RELEASE_TAG}^{commit}" in text and "workspace" in text
    assert r"(-[A-Za-z0-9.-]+)?(\+[A-Za-z0-9.-]+)?" in text
    assert 'version="${RELEASE_TAG#v}"' in text
    assert '[[ "${version%%+*}" == *-* ]]' in text
    assert "prerelease: ${{ steps.release_channel.outputs.prerelease }}" in text
    for tag, expected in (("v1.2.3-rc.1", "true"), ("v1.2.3+build-5", "false"), ("v1.2.3-rc.1+build-5", "true"), ("v1.2.3", "false")):
        result = run("bash", "-c", 'version="${1#v}"; if [[ "${version%%+*}" == *-* ]]; then printf true; else printf false; fi', "bash", tag)
        assert result.stdout == expected
    assert text.count("tar -xzf archive/dafeiyu-*.tar.gz -C binaries") == 2
    assert text.count("--binary-dir binaries") == 3
    print("PASS workflow dependencies, matrix, tag/version checks, permissions, checksums and secret hooks")


def write_elf(path: Path, machine: int = 62) -> None:
    header = bytearray(64)
    header[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHI", header, 16, 3, machine, 1)
    path.write_bytes(header)


def write_pe(path: Path, machine: int = 0x8664) -> None:
    header = bytearray(80 + 24 + 112)
    header[:2] = b"MZ"
    struct.pack_into("<I", header, 0x3C, 80)
    header[80:88] = b"PE\0\0" + struct.pack("<HH", machine, 1)
    struct.pack_into("<HH", header, 80 + 20, 112, 0x0002)
    struct.pack_into("<H", header, 80 + 24, 0x20B)
    path.write_bytes(header)


def write_mach(path: Path, cpu: int = 0x0100000C) -> None:
    header = bytearray(64)
    header[:4] = b"\xcf\xfa\xed\xfe"
    struct.pack_into("<IIII", header, 0, 0xFEEDFACF, cpu, 3, 2)
    path.write_bytes(header)


def native_fixture(directory: Path, target: str) -> Path:
    binaries = directory / ("binaries-" + target)
    binaries.mkdir(exist_ok=True)
    windows = target.endswith("windows-msvc")
    for name in (("dfy.exe", "dfy-relay.exe") if windows else ("dfy", "dfy-relay")):
        path = binaries / name
        if target == "x86_64-unknown-linux-gnu":
            write_elf(path)
        elif target == "aarch64-unknown-linux-gnu":
            write_elf(path, 183)
        elif windows:
            write_pe(path)
        elif target == "x86_64-apple-darwin":
            write_mach(path, 0x01000007)
        else:
            write_mach(path)
    return binaries


def check_archives(directory: Path) -> None:
    targets = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin")
    for target in targets:
        output = directory / target
        binaries = native_fixture(directory, target)
        args = (sys.executable, PACKAGING / "make_archive.py", "--binary-dir", binaries, "--target", target, "--version", "v1.2.3", "--output-dir", output)
        run(*args)
        suffix = "zip" if "windows" in target else "tar.gz"
        archive = output / f"dafeiyu-1.2.3-{target}.{suffix}"
        first_hash = sha(archive)
        run(*args)
        assert sha(archive) == first_hash, "archive generation is not deterministic"
        expected = {"dfy.exe", "dfy-relay.exe"} if "windows" in target else {"dfy", "dfy-relay"}
        expected |= {"README.md", "LICENSE", "DFYP-v1.md", "relay-deployment.md"}
        if suffix == "zip":
            with zipfile.ZipFile(archive) as bundle:
                assert set(bundle.namelist()) == expected
                assert bundle.testzip() is None
                for name in ("dfy.exe", "dfy-relay.exe"):
                    assert (bundle.getinfo(name).external_attr >> 16) & 0o777 == 0o755
        else:
            with tarfile.open(archive) as bundle:
                assert set(bundle.getnames()) == expected
                for member in bundle.getmembers():
                    assert member.mode == (0o755 if member.name in {"dfy", "dfy-relay"} else 0o644)
                    assert member.uid == member.gid == 0
        sidecar = archive.with_name(archive.name + ".sha256")
        assert sidecar.read_text().split()[0] == first_hash
        if "unknown-linux" in target:
            arch = target.split("-", 1)[0]
            relay = output / f"dfy-relay-linux-{arch}.tar.gz"
            with tarfile.open(relay) as bundle:
                assert bundle.getnames() == ["dfy-relay"]
                assert bundle.getmember("dfy-relay").mode == 0o755
            assert relay.with_name(relay.name + ".sha256").read_text().split()[0] == sha(relay)
    composite_bins = native_fixture(directory, targets[0])
    composite_output = directory / "composite-version"
    run(sys.executable, PACKAGING / "make_archive.py", "--binary-dir", composite_bins, "--target", targets[0], "--version", "v1.2.3-rc.1+build.5", "--output-dir", composite_output)
    assert (composite_output / f"dafeiyu-1.2.3-rc.1+build.5-{targets[0]}.tar.gz").is_file()
    run(sys.executable, PACKAGING / "make_archive.py", "--binary-dir", directory / "missing", "--target", targets[0], "--version", "1.2.3", "--output-dir", directory / "bad", ok=False)
    fabricated = directory / "fabricated"
    fabricated.mkdir()
    for name in ("dfy", "dfy-relay"):
        (fabricated / name).write_text("not a native binary\n")
    result = run(sys.executable, PACKAGING / "make_archive.py", "--binary-dir", fabricated, "--target", targets[0], "--version", "1.2.3", "--output-dir", directory / "bad", ok=False)
    assert "not an ELF executable" in result.stderr
    wrong_arch = directory / "wrong-arch"
    wrong_arch.mkdir()
    for name in ("dfy", "dfy-relay"):
        write_elf(wrong_arch / name)
    result = run(sys.executable, PACKAGING / "make_archive.py", "--binary-dir", wrong_arch, "--target", targets[1], "--version", "1.2.3", "--output-dir", directory / "bad", ok=False)
    assert "architecture mismatch" in result.stderr
    result = run(sys.executable, PACKAGING / "make_archive.py", "--binary-dir", fabricated, "--target", " invented-target", "--version", "1.2.3", "--output-dir", directory / "bad", ok=False)
    assert "invalid choice" in result.stderr
    print("PASS archive payloads, permissions, native formats, target mismatches, checksums and deterministic output")


def check_static_metadata() -> None:
    config = tomllib.loads((ROOT / "deploy/relay.toml.example").read_text(encoding="utf-8"))
    assert {"admin_addr", "max_sessions", "per_tag_mbps", "tag_ttl_hours"} <= config.keys()
    assert "relay_key" not in config and "key_file" not in config
    unit = (ROOT / "deploy/dfy-relay.service").read_text(encoding="utf-8")
    for directive in ("NoNewPrivileges=true", "ProtectSystem=strict", "User=dfy-relay", "CapabilityBoundingSet=", "RestrictAddressFamilies="):
        assert directive in unit
    assert "systemctl enable" not in (ROOT / "packaging/linux/build-packages.sh").read_text(encoding="utf-8")
    result = run("bash", "-c", 'source "$1"; validate_version "$2"', "bash", PACKAGING / "common.sh", "1.2.3-rc.1+build.5")
    assert result.stdout.strip() == "1.2.3-rc.1+build.5"
    inno = (PACKAGING / "windows/dfy.iss").read_text(encoding="utf-8")
    for required in ('Source: "{#SourceDir}\\dfy.exe"', 'Source: "{#SourceDir}\\dfy-relay.exe"', 'LicenseFile=', 'Filename: "{app}\\dfy.exe"', 'UninstallDisplayName='):
        assert required in inno, required
    assert "sc.exe" not in inno and "CreateService" not in inno
    with (PACKAGING / "macos/component.plist").open("rb") as stream:
        assert plistlib.load(stream)["BundleIsRelocatable"] is False
    mac = (PACKAGING / "macos/build-installer.sh").read_text(encoding="utf-8")
    assert "unsigned" in mac and "notarytool" in mac and "stapler" in mac
    android = (PACKAGING / "android/build.sh").read_text(encoding="utf-8")
    assert "unsigned" in android and "ANDROID_KEYSTORE_BASE64" in android
    print("PASS config, systemd, Inno, macOS and Android metadata")


def check_linux_builder(directory: Path) -> None:
    binaries = directory / "linux-binaries"
    binaries.mkdir()
    for name in ("dfy", "dfy-relay"):
        write_elf(binaries / name)
    output = directory / "linux-output"
    args = ("bash", PACKAGING / "linux/build-packages.sh", "--binary-dir", binaries, "--version", "1.2.3", "--arch", "x86_64", "--output-dir", output)
    if shutil.which("dpkg-deb"):
        run(*args, "--format", "deb")
        package = output / "dafeiyu_1.2.3_amd64.deb"
        assert package.is_file()
        fields = run("dpkg-deb", "-f", package).stdout
        assert "Package: dafeiyu" in fields and "Architecture: amd64" in fields
        payload_path = directory / "debian-payload.tar"
        with payload_path.open("wb") as stream:
            subprocess.run(["dpkg-deb", "--fsys-tarfile", str(package)], stdout=stream, check=True)
        with tarfile.open(payload_path) as bundle:
            names = set(bundle.getnames())
            assert {"./usr/bin/dfy", "./usr/bin/dfy-relay", "./usr/lib/systemd/system/dfy-relay.service", "./etc/dfy-relay/relay.toml.example"} <= names
        control_path = directory / "debian-control.tar"
        with control_path.open("wb") as stream:
            subprocess.run(["dpkg-deb", "--ctrl-tarfile", str(package)], stdout=stream, check=True)
        with tarfile.open(control_path) as bundle:
            assert {"./control", "./postinst", "./conffiles"} <= set(bundle.getnames())
        print("PASS real DEB metadata, maintainer scripts and payload")
    else:
        result = run(*args, "--format", "deb", ok=False)
        assert "missing required tool: dpkg-deb" in result.stderr
        print("PASS explicit missing-dpkg-deb failure (platform-only build not run)")
    if shutil.which("rpmbuild") and shutil.which("rpm"):
        run(*args, "--format", "rpm")
        package = next(output.glob("*.rpm"))
        listing = run("rpm", "-qp", "--dump", package).stdout
        assert "/usr/bin/dfy" in listing and "/usr/bin/dfy-relay" in listing
        print("PASS real RPM metadata and payload")
    else:
        result = run(*args, "--format", "rpm", ok=False)
        assert "missing required tool: rpmbuild" in result.stderr
        print("PASS explicit missing-rpmbuild failure (platform-only build not run)")


def write_fake_gradle(project: Path, signed: bool) -> None:
    (project / "app").mkdir(parents=True, exist_ok=True)
    wrapper = project / "gradlew"
    release_name = "app-release.apk" if signed else "app-release-unsigned.apk"
    wrapper.write_text(f'''#!/usr/bin/env bash
set -eu
printf '%s\\n' "$*" >> "$MOCK_GRADLE_LOG"
has_bundle=0
for arg in "$@"; do
  [ "$arg" != clean ] || rm -rf app/build
  [ "$arg" != bundleRelease ] || has_bundle=1
done
[ "$has_bundle" = 1 ] || exit 2
mkdir -p app/build/outputs/apk/debug app/build/outputs/apk/release app/build/outputs/bundle/release
python3 - "$PWD" "{release_name}" <<'PYTEST'
from pathlib import Path
import sys, zipfile
root = Path(sys.argv[1])
for relative in ("app/build/outputs/apk/debug/app-debug.apk", "app/build/outputs/apk/release/" + sys.argv[2], "app/build/outputs/bundle/release/app-release.aab"):
    path = root / relative
    prefix = "base/" if relative.endswith(".aab") else ""
    with zipfile.ZipFile(path, "w") as bundle:
        bundle.writestr("base/manifest/AndroidManifest.xml" if prefix else "AndroidManifest.xml", "fake test manifest")
        if not prefix:
            bundle.writestr("resources.arsc", "fake resources")
            bundle.writestr("classes.dex", "fake dex")
        for abi in ("arm64-v8a", "x86_64"):
            bundle.writestr(prefix + f"lib/{{abi}}/libdfy_android.so", b"fake JNI test payload")
PYTEST
''', encoding="utf-8")
    wrapper.chmod(0o755)
    (project / "app/build.gradle.kts").write_text('defaultConfig { versionName = "1.2.3" }\n', encoding="utf-8")


def check_android_builder(directory: Path) -> None:
    project = directory / "android-project"
    project.mkdir()
    write_fake_gradle(project, signed=False)
    output = directory / "android-output"
    args = ("bash", PACKAGING / "android/build.sh", "--android-dir", project, "--output-dir", output, "--signing", "none")
    env = os.environ.copy()
    env["MOCK_GRADLE_LOG"] = str(directory / "gradle.log")
    run(*args, env=env)
    assert (output / "dafeiyu-1.2.3-debug.apk").is_file()
    assert (output / "dafeiyu-1.2.3-release-unsigned.apk").is_file()
    assert (output / "dafeiyu-1.2.3-release-unsigned.aab").is_file()
    assert "Release APK/AAB: unsigned" in (output / "BUILD-INFO.txt").read_text()
    run(*args, "--signing", "required", ok=False, env=env)
    assert "clean assembleDebug assembleRelease bundleRelease" in (directory / "gradle.log").read_text()
    print("PASS Android clean APK/AAB tasks, unsigned labels and required-signing failure via fake Gradle wrapper")

    # Exercise the signed path without real keys: apksigner is a local test double.
    write_fake_gradle(project, signed=True)
    sdk = directory / "fake-sdk"
    tools = sdk / "build-tools/36.0.0"
    tools.mkdir(parents=True)
    verifier = tools / "apksigner"
    verifier.write_text('#!/usr/bin/env bash\n[ -s "$3" ] && printf "verified by test double\\n"\n', encoding="utf-8")
    verifier.chmod(0o755)
    jar_verifier = tools / "jarsigner"
    jar_verifier.write_text('#!/usr/bin/env bash\n[ "$1" = -verify ] && [ -s "$2" ]\n', encoding="utf-8")
    jar_verifier.chmod(0o755)
    keystore = directory / "test-only.jks"
    keystore.write_bytes(b"not a real keystore")
    signed_output = directory / "android-signed-output"
    env = os.environ.copy()
    env.update({"ANDROID_KEYSTORE_PATH": str(keystore), "ANDROID_KEYSTORE_PASSWORD": "test", "ANDROID_KEY_ALIAS": "test", "ANDROID_KEY_PASSWORD": "test", "ANDROID_SDK_ROOT": str(sdk), "MOCK_GRADLE_LOG": str(directory / "gradle.log"), "PATH": str(tools) + os.pathsep + env["PATH"]})
    run("bash", PACKAGING / "android/build.sh", "--android-dir", project, "--output-dir", signed_output, "--signing", "auto", env=env)
    assert (signed_output / "dafeiyu-1.2.3-release.apk").is_file()
    assert "Release APK/AAB: signed" in (signed_output / "BUILD-INFO.txt").read_text()
    print("PASS Android signed artifact selection/metadata via test doubles")


def check_platform_builder_failures(directory: Path) -> None:
    windows_bins = directory / "windows-binaries"
    windows_bins.mkdir()
    for name in ("dfy.exe", "dfy-relay.exe"):
        write_pe(windows_bins / name)
    windows_args = ("bash", PACKAGING / "windows/build-installer.sh", "--binary-dir", windows_bins, "--version", "1.2.3", "--output-dir", directory / "windows-output")
    local_iscc = Path(os.environ.get("LOCALAPPDATA", "")) / "Programs/Inno Setup 6/ISCC.exe"
    if not (shutil.which("ISCC.exe") or shutil.which("iscc") or Path("/c/Program Files (x86)/Inno Setup 6/ISCC.exe").exists() or local_iscc.exists()):
        result = run(*windows_args, ok=False)
        assert "missing required tool: Inno Setup 6 ISCC.exe" in result.stderr
        print("PASS explicit missing-Inno-Setup failure (installer build not run on this host)")
    unix_bins = directory / "macos-binaries"
    unix_bins.mkdir()
    for name in ("dfy", "dfy-relay"):
        write_mach(unix_bins / name)
    mac_args = ("bash", PACKAGING / "macos/build-installer.sh", "--binary-dir", unix_bins, "--version", "1.2.3", "--arch", "arm64", "--output-dir", directory / "macos-output")
    if not shutil.which("pkgbuild"):
        result = run(*mac_args, ok=False)
        assert "missing required tool: pkgbuild" in result.stderr
        print("PASS explicit missing-pkgbuild failure (macOS installer build not run on this host)")


def executable(path: Path, source: str) -> None:
    path.write_text(source, encoding="utf-8")
    path.chmod(0o755)


def check_mocked_installers(directory: Path) -> None:
    tools = directory / "mock-macos-tools"
    tools.mkdir()
    log = directory / "macos-tools.log"
    for name in ("pkgbuild", "productbuild"):
        executable(tools / name, '#!/usr/bin/env bash\nset -eu\nprintf "mock package\\n" > "${@: -1}"\n')
    executable(tools / "hdiutil", '''#!/usr/bin/env bash
set -eu
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = -ov ]; then out="$2"; shift 2; else shift; fi
done
[ -n "$out" ]
printf 'mock dmg\n' > "$out"
''')
    executable(tools / "shasum", '#!/usr/bin/env bash\nprintf "%064d  %s\\n" 0 "${@: -1}"\n')
    executable(tools / "codesign", '#!/usr/bin/env bash\nprintf "codesign %s\\n" "$*" >> "$MOCK_TOOL_LOG"\n')
    executable(tools / "xcrun", '''#!/usr/bin/env bash
set -eu
printf 'xcrun %s\n' "$*" >> "$MOCK_TOOL_LOG"
if [ "$1" = --find ]; then printf '/mock/notarytool\n'; fi
''')
    binaries = native_fixture(directory, "aarch64-apple-darwin")
    output = directory / "macos-mock-output"
    args = ("bash", PACKAGING / "macos/build-installer.sh", "--binary-dir", binaries, "--version", "1.2.3", "--arch", "arm64", "--format", "all", "--output-dir", output)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("MAC_") or key.startswith("APPLE_"):
            env.pop(key)
    env.update({"PATH": str(tools) + os.pathsep + env["PATH"], "MOCK_TOOL_LOG": str(log)})
    # GitHub sets secret-backed env keys even when their values are empty.
    for key in ("MAC_SIGNING_IDENTITY", "MAC_DMG_SIGNING_IDENTITY", "MAC_NOTARY_KEY_BASE64", "MAC_NOTARY_KEY_ID", "MAC_NOTARY_ISSUER_ID", "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID", "MAC_REQUIRE_NOTARIZATION"):
        env[key] = ""
    run(*args, env=env)
    assert (output / "dafeiyu-1.2.3-macos-arm64-unsigned.pkg").is_file()
    assert (output / "dafeiyu-1.2.3-macos-arm64-unsigned.dmg").is_file()
    assert "Signing: unsigned" in (output / "BUILD-INFO-macos-arm64.txt").read_text()
    assert "Target: aarch64-apple-darwin" in (output / "BUILD-INFO-macos-arm64.txt").read_text()
    assert not log.exists(), "unsigned build invoked signing/notarization tools"
    required = env.copy()
    required["MAC_REQUIRE_NOTARIZATION"] = "1"
    result = run(*args, "--output-dir", directory / "mac-required", env=required, ok=False)
    assert "no complete notarization credentials" in result.stderr
    signed = env.copy()
    signed.update({"MAC_SIGNING_IDENTITY": "Developer ID Installer", "APPLE_ID": "test@example.invalid", "APPLE_PASSWORD": "test", "APPLE_TEAM_ID": "TEST", "MAC_REQUIRE_NOTARIZATION": "1"})
    signed_output = directory / "macos-signed-mock-output"
    run(*args, "--output-dir", signed_output, env=signed)
    assert (signed_output / "dafeiyu-1.2.3-macos-arm64-signed.pkg").is_file()
    assert "signed and notarized/stapled" in (signed_output / "BUILD-INFO-macos-arm64.txt").read_text()
    assert log.read_text().count("xcrun stapler staple") == 2
    signed_only = env.copy()
    signed_only["MAC_SIGNING_IDENTITY"] = "Developer ID Installer"
    signed_only_output = directory / "macos-signed-only-output"
    run(*args, "--format", "pkg", "--output-dir", signed_only_output, env=signed_only)
    assert "signed (not notarized)" in (signed_only_output / "BUILD-INFO-macos-arm64.txt").read_text()
    dmg_only = env.copy()
    dmg_only["MAC_DMG_SIGNING_IDENTITY"] = "Developer ID Application"
    result = run(*args, "--output-dir", directory / "mac-dmg-only", env=dmg_only, ok=False)
    assert "requires MAC_SIGNING_IDENTITY" in result.stderr
    print("PASS mocked macOS PKG/DMG empty-secret, signing and notarization honesty")

    windows_tools = directory / "mock-windows-tools"
    windows_tools.mkdir()
    executable(windows_tools / "cygpath", '#!/usr/bin/env bash\nprintf "%s\\n" "${@: -1}"\n')
    executable(windows_tools / "iscc", '''#!/usr/bin/env bash
set -eu
source_dir="" output_dir="" version=""
for arg in "$@"; do
  case "$arg" in
    /DSourceDir=*) source_dir="${arg#*=}" ;;
    /DOutputDir=*) output_dir="${arg#*=}" ;;
    /DAppVersion=*) version="${arg#*=}" ;;
  esac
done
for name in dfy.exe dfy-relay.exe docs/LICENSE docs/relay-deployment.md; do
  [ -f "$source_dir/$name" ]
done
printf 'mock installer' > "$output_dir/dafeiyu-$version-windows-x86_64-setup.exe"
''')
    windows_bins = native_fixture(directory, "x86_64-pc-windows-msvc")
    windows_output = directory / "windows-mock-output"
    windows_env = env.copy()
    windows_env.pop("ISCC", None)
    windows_env["PATH"] = str(windows_tools) + os.pathsep + env["PATH"]
    run("bash", PACKAGING / "windows/build-installer.sh", "--binary-dir", windows_bins, "--version", "1.2.3", "--output-dir", windows_output, env=windows_env)
    assert (windows_output / "dafeiyu-1.2.3-windows-x86_64-setup.exe").is_file()
    write_pe(windows_bins / "dfy.exe", 0xAA64)
    result = run("bash", PACKAGING / "windows/build-installer.sh", "--binary-dir", windows_bins, "--version", "1.2.3", "--output-dir", directory / "windows-wrong-arch", env=windows_env, ok=False)
    assert "architecture mismatch" in result.stderr
    print("PASS mocked Windows Inno payload and PE architecture rejection")


def check_mocked_linux_packages(directory: Path) -> None:
    tools = directory / "mock-linux-tools"
    tools.mkdir()
    executable(tools / "dpkg-deb", '''#!/usr/bin/env bash
set -eu
[ "$1" = --root-owner-group ] && [ "$2" = --build ]
tar --force-local -czf "$4" -C "$3" .
''')
    executable(tools / "rpmbuild", '''#!/usr/bin/env bash
set -eu
spec=""
top=""
while [ "$#" -gt 0 ]; do
  spec="$1"
  if [ "$1" = --define ]; then shift; top="${1#_topdir }"; fi
  shift
done
[ -n "$top" ]
cp "$spec" "$MOCK_SPEC_COPY"
mkdir -p "$top/RPMS/x86_64"
printf 'mock rpm\n' > "$top/RPMS/x86_64/dafeiyu-1.2.3-1.x86_64.rpm"
''')
    binaries = native_fixture(directory, "x86_64-unknown-linux-gnu")
    output = directory / "linux-mock-output"
    env = os.environ.copy()
    env.update({"PATH": str(tools) + os.pathsep + env["PATH"], "MOCK_SPEC_COPY": str(directory / "dafeiyu.spec")})
    run("bash", PACKAGING / "linux/build-packages.sh", "--binary-dir", binaries, "--version", "1.2.3", "--arch", "x86_64", "--format", "all", "--output-dir", output, env=env)
    with tarfile.open(output / "dafeiyu_1.2.3_amd64.deb") as bundle:
        names = set(bundle.getnames())
        assert {"./usr/bin/dfy", "./usr/bin/dfy-relay", "./etc/dfy-relay/relay.toml.example", "./usr/share/doc/dafeiyu/LICENSE"} <= names
        if os.name != "nt":
            assert bundle.getmember("./usr/bin/dfy").mode == 0o755
            assert bundle.getmember("./usr/bin/dfy-relay").mode == 0o755
    assert "%config(noreplace)" in (directory / "dafeiyu.spec").read_text()
    manifest = (output / "SHA256SUMS-linux-x86_64-all.txt").read_text().splitlines()
    assert len(manifest) == 2
    for line in manifest:
        parts = line.split()
        assert len(parts) == 2, repr(line)
        expected, name = parts
        name = name.removeprefix("*")
        assert Path(name).name == name and (output / name).is_file(), repr(name)
        assert hashlib.sha256((output / name).read_bytes()).hexdigest() == expected
    write_elf(binaries / "dfy", 183)
    result = run("bash", PACKAGING / "linux/build-packages.sh", "--binary-dir", binaries, "--version", "1.2.3", "--arch", "x86_64", "--format", "deb", "--output-dir", directory / "linux-wrong-arch", env=env, ok=False)
    assert "architecture mismatch" in result.stderr
    print("PASS mocked DEB/RPM payloads, conffile policy, portable checksum manifest and ELF mismatch (Unix modes require a POSIX host)")


def main() -> None:
    check_syntax()
    check_workflow()
    check_static_metadata()
    with tempfile.TemporaryDirectory(prefix="dfy-packaging-smoke-") as temporary:
        directory = Path(temporary)
        check_archives(directory)
        check_linux_builder(directory)
        check_platform_builder_failures(directory)
        check_mocked_linux_packages(directory)
        check_mocked_installers(directory)
        check_android_builder(directory)
    print("ALL HOST-RUNNABLE PACKAGING CHECKS PASSED")


if __name__ == "__main__":
    main()
