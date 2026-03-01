# Copyright 2013 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

import datetime
import os
from os import PathLike
import os.path as path
import pathlib
import shutil
import stat
import sys

from time import time
from typing import Union, Any

from mach.decorators import (
    CommandArgument,
    CommandProvider,
    Command,
)
from mach.registrar import Registrar

import servo.platform
import servo.platform.macos
import servo.util
import servo.visual_studio

from servo.command_base import BuildType, CommandBase, check_call
from servo.gstreamer import windows_dlls, windows_plugins, package_gstreamer_dylibs
from servo.platform.build_target import BuildTarget

from python.servo.platform.build_target import SanitizerKind


@CommandProvider
class MachCommands(CommandBase):
    @Command("build", description="Build Servo", category="build")
    @CommandArgument("--jobs", "-j", default=None, help="Number of jobs to run in parallel")
    @CommandArgument(
        "--no-package", action="store_true", help="For Android, disable packaging into a .apk after building"
    )
    @CommandArgument("--verbose", "-v", action="store_true", help="Print verbose output")
    @CommandArgument("--very-verbose", "-vv", action="store_true", help="Print very verbose output")
    @CommandArgument("params", nargs="...", help="Command-line arguments to be passed through to Cargo")
    @CommandBase.common_command_arguments(build_configuration=True, build_type=True, package_configuration=True)
    def build(
        self,
        build_type: BuildType,
        jobs: str | None = None,
        params: list[str] | None = None,
        no_package: bool = False,
        verbose: bool = False,
        very_verbose: bool = False,
        sanitizer: SanitizerKind = SanitizerKind.NONE,
        flavor: str | None = None,
        **kwargs: Any,
    ) -> int:
        opts = params or []

        if build_type.is_release():
            opts += ["--release"]
        elif build_type.is_dev():
            pass  # there is no argument for debug
        else:
            opts += ["--profile", build_type.profile]

        if jobs is not None:
            opts += ["-j", jobs]
        if verbose:
            opts += ["-v"]
        if very_verbose:
            opts += ["-vv"]
        self.config["build"]["sanitizer"] = sanitizer

        env = self.build_env(sanitizer=sanitizer)
        self.ensure_bootstrapped()

        host = servo.platform.host_triple()
        target_triple = self.target.triple()

        if self.enable_code_coverage:
            print("Building with code coverage instrumentation...")
            # We don't want coverage for build-scripts and proc macros.
            kwargs["target_override"] = target_triple
            this_dir = pathlib.Path(os.path.dirname(__file__))
            servo_root_dir = this_dir.parent.parent
            coverage_workspace_wrapper = servo_root_dir.joinpath("etc/coverage_workspace_wrapper.py")
            if not coverage_workspace_wrapper.exists():
                print(
                    f"Could not find rustc workspace wrapper script at expected location: {coverage_workspace_wrapper}"
                )
            env["RUSTC_WORKSPACE_WRAPPER"] = str(coverage_workspace_wrapper)

        if sanitizer.is_some():
            # std library should also be instrumented
            opts += ["-Zbuild-std"]
            # We need to always set the target triple, even when building for host.
            kwargs["target_override"] = target_triple
        build_start = time()

        if host != target_triple and "windows" in target_triple:
            if os.environ.get("VisualStudioVersion") or os.environ.get("VCINSTALLDIR"):
                print(
                    "Can't cross-compile for Windows inside of a Visual Studio shell.\n"
                    "Please run `python mach build [arguments]` to bypass automatic "
                    "Visual Studio shell, and make sure the VisualStudioVersion and "
                    "VCINSTALLDIR environment variables are not set."
                )
                sys.exit(1)

        # Gather Cargo build timings (https://doc.rust-lang.org/cargo/reference/timings.html).
        opts = ["--timings"] + opts

        crown_enabled = "enabled" if kwargs.get("use_crown", False) else "disabled (no JS garbage collection linting)"
        print(f"Building `{build_type.directory_name()}` build with crown {crown_enabled}.")
        if very_verbose:
            for key in env:
                print((key, env[key]))

        status = self.run_cargo_build_like_command("rustc", opts, env=env, verbose=verbose, **kwargs)

        if status == 0:
            if not no_package and self.target.needs_packaging():
                return_value = Registrar.dispatch(
                    "package", context=self.context, build_type=build_type, flavor=flavor, sanitizer=sanitizer
                )
                if return_value:
                    return return_value

            return_value = self.run_post_build_tasks(build_type, sanitizer)
            if return_value:
                return return_value

        # Print how long the build took
        elapsed = time() - build_start
        elapsed_delta = datetime.timedelta(seconds=int(elapsed))
        build_message = f"{'Succeeded' if status == 0 else 'Failed'} in {elapsed_delta}"
        print(build_message)
        assert isinstance(status, int)
        return status

    @Command("run-post-build-tasks", description="Run only the post-build tasks", category="build")
    @CommandBase.common_command_arguments(build_configuration=True, build_type=True)
    def run_post_build_tasks_cmd(
        self,
        build_type: BuildType,
        sanitizer: SanitizerKind = SanitizerKind.NONE,
        **_kwargs: Any,
    ) -> int:
        return self.run_post_build_tasks(build_type, sanitizer)

    def run_post_build_tasks(self, build_type: BuildType, sanitizer: SanitizerKind = SanitizerKind.NONE) -> int:
        target_triple = self.target.triple()

        built_binary = self.get_binary_path(build_type, sanitizer=sanitizer)
        binary_dir = os.path.dirname(built_binary)
        assert os.path.exists(binary_dir)

        if "windows" in target_triple:
            if not copy_windows_dlls_to_build_directory(built_binary, self.target):
                return 1

        elif "darwin" in target_triple:
            servo_bin_dir = os.path.dirname(built_binary)
            assert os.path.exists(servo_bin_dir)

            if self.enable_media:
                library_target_directory = path.join(path.dirname(built_binary), "lib/")
                if not package_gstreamer_dylibs(built_binary, library_target_directory, self.target):
                    return 1

            # On the Mac, set a lovely icon. This makes it easier to pick out the Servo binary in tools
            # like Instruments.app.
            try:
                import Cocoa  # pyrefly: ignore[import-error]

                icon_path = path.join(self.get_top_dir(), "resources", "servo_1024.png")
                icon = Cocoa.NSImage.alloc().initWithContentsOfFile_(icon_path)
                if icon is not None:
                    Cocoa.NSWorkspace.sharedWorkspace().setIcon_forFile_options_(icon, built_binary, 0)
            except ImportError:
                pass
        return 0

    @Command("clean", description="Clean the target/ and Python virtual environment directories", category="build")
    @CommandArgument("--manifest-path", default=None, help="Path to the manifest to the package to clean")
    @CommandArgument("--verbose", "-v", action="store_true", help="Print verbose output")
    @CommandArgument("params", nargs="...", help="Command-line arguments to be passed through to Cargo")
    def clean(self, manifest_path: str | None = None, params: list[str] = [], verbose: bool = False) -> None:
        self.ensure_bootstrapped()

        virtualenv_path = path.join(self.get_top_dir(), ".venv")
        if path.exists(virtualenv_path):
            print("Removing virtualenv directory: %s" % virtualenv_path)
            shutil.rmtree(virtualenv_path)

        opts = ["--manifest-path", manifest_path or path.join(self.context.topdir, "Cargo.toml")]
        if verbose:
            opts += ["-v"]
        opts += params
        return check_call(["cargo", "clean"] + opts, env=self.build_env(), verbose=verbose)

def copy_windows_dlls_to_build_directory(servo_binary: str, target: BuildTarget) -> bool:
    servo_exe_dir = os.path.dirname(servo_binary)
    assert os.path.exists(servo_exe_dir)

    build_path = path.join(servo_exe_dir, "build")
    assert os.path.exists(build_path)

    # Copy in the built EGL and GLES libraries from where they were built to
    # the final build dirctory
    def find_and_copy_built_dll(dll_name: str) -> None:
        try:
            file_to_copy = next(pathlib.Path(build_path).rglob(dll_name))
            shutil.copy(file_to_copy, servo_exe_dir)
        except StopIteration:
            print(f"WARNING: could not find {dll_name}")

    print(" • Copying ANGLE DLLs to binary directory...")
    find_and_copy_built_dll("libEGL.dll")
    find_and_copy_built_dll("libGLESv2.dll")

    print(" • Copying GStreamer DLLs to binary directory...")
    if not package_gstreamer_dlls(servo_exe_dir, target):
        return False

    print(" • Copying MSVC DLLs to binary directory...")
    if not package_msvc_dlls(servo_exe_dir, target):
        return False

    return True


def package_gstreamer_dlls(servo_exe_dir: str, target: BuildTarget) -> bool:
    gst_root = servo.platform.get().gstreamer_root(target)
    if not gst_root:
        print("Could not find GStreamer installation directory.")
        return False

    missing = []
    for gst_lib in windows_dlls():
        try:
            shutil.copy(path.join(gst_root, "bin", gst_lib), servo_exe_dir)
        except Exception:
            missing += [str(gst_lib)]

    for gst_lib in missing:
        print("ERROR: could not find required GStreamer DLL: " + gst_lib)
    if missing:
        return False

    # Only copy a subset of the available plugins.
    gst_dlls = windows_plugins()

    gst_plugin_path_root = os.environ.get("GSTREAMER_PACKAGE_PLUGIN_PATH") or gst_root
    gst_plugin_path = path.join(gst_plugin_path_root, "lib", "gstreamer-1.0")
    if not os.path.exists(gst_plugin_path):
        print("ERROR: couldn't find gstreamer plugins at " + gst_plugin_path)
        return False

    missing = []
    for gst_lib in gst_dlls:
        try:
            shutil.copy(path.join(gst_plugin_path, gst_lib), servo_exe_dir)
        except Exception:
            missing += [str(gst_lib)]

    for gst_lib in missing:
        print("ERROR: could not find required GStreamer DLL: " + gst_lib)
    return not missing


def package_msvc_dlls(servo_exe_dir: str, target: BuildTarget) -> bool:
    def copy_file(dll_path: Union[PathLike[str], str]) -> bool:
        if not dll_path or not os.path.exists(dll_path):
            print(f"WARNING: Could not find DLL at {dll_path}", file=sys.stderr)
            return False
        servo_dir_dll = path.join(servo_exe_dir, os.path.basename(dll_path))
        # Avoid permission denied error when overwriting DLLs.
        if os.path.isfile(servo_dir_dll):
            os.chmod(servo_dir_dll, stat.S_IWUSR)
        print(f"    • Copying {dll_path}")
        shutil.copy(dll_path, servo_exe_dir)
        return True

    vs_platform = {
        "x86_64": "x64",
        "i686": "x86",
        "aarch64": "arm64",
    }[target.triple().split("-")[0]]

    for msvc_redist_dir in servo.visual_studio.find_msvc_redist_dirs(vs_platform):
        if copy_file(os.path.join(msvc_redist_dir, "msvcp140.dll")) and copy_file(
            os.path.join(msvc_redist_dir, "vcruntime140.dll")
        ):
            break

    # Different SDKs install the file into different directory structures within the
    # Windows SDK installation directory, so use a glob to search for a path like
    # "**\x64\api-ms-win-crt-runtime-l1-1-0.dll".
    windows_sdk_dir = servo.visual_studio.find_windows_sdk_installation_path()
    dll_name = "api-ms-win-crt-runtime-l1-1-0.dll"
    file_to_copy = next(pathlib.Path(windows_sdk_dir).rglob(os.path.join("**", vs_platform, dll_name)))
    copy_file(file_to_copy)

    return True
