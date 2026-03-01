# Copyright 2026 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

import shutil
import subprocess
import sys
from typing import Any, Optional

from python.servo.platform.build_target import SanitizerKind

SUPPORTED_ASAN_TARGETS = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-ohos",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
]

SUPPORTED_TSAN_TARGETS = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
]


def get_rustc_llvm_version() -> Optional[list[int]]:
    """Determine the LLVM version of `rustc` and return it as a List[major, minor, patch, ...]"""
    try:
        result = subprocess.run(["rustc", "--version", "--verbose"], encoding="utf-8", capture_output=True)
        result.check_returncode()
        for line in result.stdout.splitlines():
            line_lowercase = line.lower()
            if line_lowercase.startswith("llvm version:"):
                llvm_version = line_lowercase.strip("llvm version:")
                llvm_version = llvm_version.strip()
                version = llvm_version.split(".")
                print(f"Info: rustc is using LLVM version {'.'.join(version)}")
                return list(map(int, version))
        else:
            print(f"Error: Couldn't find LLVM version in output of `rustc --version --verbose`: `{result.stdout}`")
    except Exception as e:
        print(f"Error: Failed to determine rustc version: {e}")
    return None


def configure_sanitizer_environment(
    env: dict[str, str], target_triple: str, sanitizer: SanitizerKind, features: list[str]
) -> None:
    if sanitizer.is_none():
        return
    # do not use crown (clashes with different rust version)
    env["RUSTC"] = "rustc"
    # Enable usage of unstable rust flags
    env["RUSTC_BOOTSTRAP"] = "1"
    # When sanitizers are used we also want framepointers to help with backtraces.
    if "force-frame-pointers" not in env.get("RUSTFLAGS", ""):
        env["RUSTFLAGS"] = env.get("RUSTFLAGS", "") + " -C force-frame-pointers=yes"

    # Note: We want to use the same clang/LLVM version as rustc.
    rustc_llvm_version = get_rustc_llvm_version()
    if rustc_llvm_version is None:
        raise RuntimeError("Unable to determine necessary clang version for Sanitizer support")
    llvm_major: int = rustc_llvm_version[0]
    target_clang = f"clang-{llvm_major}"
    target_cxx = f"clang++-{llvm_major}"
    if shutil.which(target_clang) is None or shutil.which(target_cxx) is None:
        env.setdefault("TARGET_CC", "clang")
        env.setdefault("TARGET_CXX", "clang++")
    else:
        # libasan can be compatible across multiple compiler versions and has a
        # runtime check, which would fail if we used incompatible compilers, so
        # we can try and fallback to the default clang.
        env.setdefault("TARGET_CC", target_clang)
        env.setdefault("TARGET_CXX", target_cxx)
    # By default, build mozjs from source to enable Sanitizers in mozjs.
    env.setdefault("MOZJS_FROM_SOURCE", "1")

    # We need to use `TARGET_CFLAGS`, since we don't want to compile host dependencies with ASAN,
    # since that causes issues when building build-scripts / proc macros.
    # The actual flags will be appended below depending on the sanitizer kind.
    env.setdefault("TARGET_CFLAGS", "")
    env.setdefault("TARGET_CXXFLAGS", "")
    env.setdefault("RUSTFLAGS", "")

    if sanitizer.is_asan():
        if target_triple not in SUPPORTED_ASAN_TARGETS:
            print(
                "AddressSanitizer is currently not supported on this platform\n",
                "See https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html",
            )
            sys.exit(1)

        # Enable asan
        env["RUSTFLAGS"] += " -Zsanitizer=address"
        env["TARGET_CFLAGS"] += " -fsanitize=address"
        env["TARGET_CXXFLAGS"] += " -fsanitize=address"

        # Set servo style thread stack size to 8 MB for ASAN builds since the stack usage is higher.
        # We don't care about efficiency, we just want to avoid crashes.
        env["SERVO_STYLE_THREAD_STACK_SIZE_KB"] = str(1024 * 8)

        # asan replaces system allocator with asan allocator
        # we need to make sure that we do not replace it with jemalloc
        if "servo_allocator/use-system-allocator" not in features:
            features.append("servo_allocator/use-system-allocator")
    elif sanitizer.is_tsan():
        if target_triple not in SUPPORTED_TSAN_TARGETS:
            print(
                "ThreadSanitizer is currently not supported on this platform\n",
                "See https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html",
            )
            sys.exit(1)
        env["RUSTFLAGS"] += " -Zsanitizer=thread"
        env["TARGET_CFLAGS"] += " -fsanitize=thread"
        env["TARGET_CXXFLAGS"] += " -fsanitize=thread"
