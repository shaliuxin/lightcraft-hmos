#!/usr/bin/env bash
# Build the LightCraft Rust static libraries (every ABI in LIGHTCRAFT_NATIVE_TARGETS) before
# hvigor/CMake. Runs on Windows (Git Bash) or macOS/Linux.
#
# Windows cross-compile rule (do NOT break this): the OHOS NDK ships <triple>-clang as a Unix
# shell script on Windows (fails with "not a valid Win32 application"). The C compiler, archiver
# and linker MUST be the PE binaries clang.exe / llvm-ar.exe, which carry their own OHOS sysroot.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -f "$ROOT/scripts/dev.local.env" ]]; then source "$ROOT/scripts/dev.local.env"; fi

# Locate the HarmonyOS native SDK (Windows DevEco Studio layout, or a macOS one).
SDK="${LIGHTCRAFT_NATIVE_SDK:-}"
if [[ -z "$SDK" ]]; then
  for candidate in \
    "/c/Program Files/Huawei/DevEco Studio/sdk/default/openharmony/native" \
    "/c/Program Files/Huawei/DevEco-Studio/sdk/default/openharmony/native" \
    "/Applications/DevEco-Studio.app/Contents/sdk/default/openharmony/native" \
    "${DEVECO_SDK_HOME:-}/default/openharmony/native" \
    "${HOS_SDK_HOME:-}"; do
    if [[ -n "$candidate" && -d "$candidate/llvm" && -d "$candidate/sysroot" ]]; then SDK="$candidate"; break; fi
  done
fi
[[ -n "$SDK" && -d "$SDK/llvm/bin" ]] || { echo "HarmonyOS native SDK not found (set LIGHTCRAFT_NATIVE_SDK)" >&2; exit 1; }

# Pick the PE binaries on Windows; the bare names elsewhere.
if [[ -x "$SDK/llvm/bin/clang.exe" ]]; then
  CC_BIN="$SDK/llvm/bin/clang.exe"
  AR_BIN="$SDK/llvm/bin/llvm-ar.exe"
else
  CC_BIN="$SDK/llvm/bin/clang"
  AR_BIN="$SDK/llvm/bin/llvm-ar"
fi
[[ -x "$CC_BIN" ]] || { echo "OHOS clang not usable: $CC_BIN" >&2; exit 1; }

# CJK font: embed the bundled craft-fonts so the zh-hans / zh-hant UI renders (without it every
# Chinese glyph is an empty box). Point at a real craft-fonts checkout to override.
CRAFT_FONTS="${CRAFT_FONTS_DIR:-$ROOT/craft-fonts}"
if [[ -f "$CRAFT_FONTS/fonts/manifest.txt" ]]; then
  echo "=== CJK font dir: $CRAFT_FONTS ===" >&2
else
  echo "=== WARNING: $CRAFT_FONTS/fonts/manifest.txt missing -> Chinese will render as boxes ===" >&2
fi

# Both ABIs ship so one HAP runs on ARM devices and the x86_64 phone/PC emulator images.
TARGETS="${LIGHTCRAFT_NATIVE_TARGETS:-aarch64-unknown-linux-ohos x86_64-unknown-linux-ohos}"
PROFILE="${LIGHTCRAFT_RUST_PROFILE:-release}"
case "$PROFILE" in
  release) ARGS+=(--release);;
  debug) ARGS+=(--profile dev);;
  *) echo 'LIGHTCRAFT_RUST_PROFILE must be release or debug' >&2; exit 2;;
esac

for target in $TARGETS; do
  case "$target" in
    aarch64-unknown-linux-ohos|x86_64-unknown-linux-ohos|armv7-unknown-linux-ohos) ;;
    *) echo "Unsupported target: $target" >&2; exit 2;;
  esac
  [[ -d "$(rustc --print sysroot)/lib/rustlib/$target" ]] || { echo "Install target: rustup target add $target" >&2; exit 1; }
  suffix="${target//-/_}"
  upper="$(printf '%s' "$suffix" | tr '[:lower:]' '[:upper:]')"
  echo "=== building $target ($PROFILE) ===" >&2
  env "CC_${suffix}=$CC_BIN" "AR_${suffix}=$AR_BIN" \
      "CARGO_TARGET_${upper}_AR=$AR_BIN" "CARGO_TARGET_${upper}_CC=$CC_BIN" \
      "CARGO_TARGET_${upper}_LINKER=$CC_BIN" \
      "CRAFT_FONTS_DIR=$CRAFT_FONTS" \
      cargo build --locked --manifest-path "$ROOT/native/rust/Cargo.toml" --target "$target" "${ARGS[@]}"
done

echo "=== native libraries ready ===" >&2
for target in $TARGETS; do
  lib="$ROOT/native/rust/target/$target/$PROFILE/liblightcraft_hmos.a"
  if [[ -f "$lib" ]]; then
    echo "  $target: $(du -h "$lib" | cut -f1)  $lib" >&2
  else
    echo "  $target: MISSING $lib" >&2
  fi
done
