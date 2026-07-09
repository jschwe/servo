# mypy: allow-untyped-defs

import atexit
import os
import subprocess
import tempfile
import threading
import time

import requests

from tools.serve.serve import make_hosts_file

from .base import (Browser,
                   ExecutorBrowser,
                   require_arg)
from .base import get_timeout_multiplier  # noqa: F401
from ..executors import executor_kwargs as base_executor_kwargs
from ..executors.base import PytestExecutor  # noqa: F401
from ..executors.executorservo import (ServoTestharnessExecutor,  # noqa: F401
                                       ServoRefTestExecutor,  # noqa: F401
                                       ServoCrashtestExecutor)  # noqa: F401

here = os.path.dirname(__file__)

__wptrunner__ = {
    "product": "servo_ohos",
    "check_args": "check_args",
    "browser": "ServoOhosBrowser",
    "executor": {
        "testharness": "ServoTestharnessExecutor",
        "reftest": "ServoRefTestExecutor",
        "crashtest": "ServoCrashtestExecutor",
        "wdspec": "PytestExecutor",
        "aamtest": "PytestExecutor",
        "test262": "ServoTestharnessExecutor",
    },
    "browser_kwargs": "browser_kwargs",
    "executor_kwargs": "executor_kwargs",
    "env_extras": "env_extras",
    "env_options": "env_options",
    "timeout_multiplier": "get_timeout_multiplier",
    "update_properties": "update_properties",
}

# OHOS bundle and ability defaults. The bundle is overridable via the shared
# wptrunner `--package-name` flag; the ability name is fixed.
DEFAULT_PACKAGE = "org.servo.servo"
DEFAULT_ABILITY = "EntryAbility"
# Name of the HAP module inside the Servo bundle. Hard-coded here because it
# determines the sandbox path layout (see DEVICE_FILES_DIR below) and is
# declared in `support/openharmony/entry/src/main/module.json5`. Note that
# despite the source-tree path being `support/openharmony/entry/`, the HAP
# module name is `servoshell`.
HAP_MODULE = "servoshell"

# Where we land WPT fixtures (hosts file, CA cert) inside the Servo app sandbox.
# Files can be pushed here via `hdc file send -b org.servo.servo`.
DEVICE_FILES_DIR = f"/data/storage/el2/base/haps/{HAP_MODULE}/files"

# hilog domain Servo logs under (see `hilog::LogDomain::new(0xE0C3)` in
# ports/servoshell/egl/ohos/mod.rs). servoshell also redirects stdout/stderr
# into hilog under this domain, so Rust panic output shows up here too. hilog
# is a system-wide firehose, so we filter to this domain at the source with
# `hilog -D`. 0xE0C3 is an "app" domain (hilog's app range is [0x0, 0xffff]),
# so it's passed as-is — no 0xD0 OS-log prefix. Set SERVO_OHOS_WPT_HILOG_ALL=1
# to stream everything instead (e.g. to chase a native crash that logs outside
# Servo's domain).
SERVO_HILOG_DOMAIN = "0xE0C3"

# Where OHOS DFX (FaultLogger) stores fault reports. When a process receives a
# fatal signal, ProcessDump writes a report here (named
# `<type>-<process name>-<uid>-<ms timestamp>.log`) with the crashing thread's
# native backtrace. Servo's `--hard-fail` panic hook raises SIGSEGV on panic,
# so Rust panics produce a cppcrash report too (the panic message itself goes
# to hilog). The directory keeps at most 10 reports per process.
FAULTLOG_DIR = "/data/log/faultlog/faultlogger"
# Report types worth attributing to the browser: native crashes, watchdog
# kills, and ArkTS crashes of the host ability.
FAULTLOG_PREFIXES = ("cppcrash", "appfreeze", "jscrash")
# cppcrash reports continue with per-thread backtraces of every other thread,
# register/memory/maps dumps etc. — hundreds of KB for a process with as many
# threads as Servo. Everything up to this marker (fault thread backtrace and
# registers) is the useful part for a test log.
FAULTLOG_TRUNCATE_MARKER = "Other thread info:"
FAULTLOG_MAX_CHARS = 16000
# How long to wait for ProcessDump/HiView to write the report after the app
# died. Reports usually appear within a couple of seconds.
FAULTLOG_WRITE_DEADLINE = 8.0
FAULTLOG_POLL_INTERVAL = 0.5


def check_args(**kwargs):
    require_arg(kwargs, "webdriver_port")


def browser_kwargs(logger, test_type, run_info_data, config, subsuite, **kwargs):
    binary_args = kwargs["binary_args"] + subsuite.config.get("binary_args", [])
    if test_type == "reftest":
        # Reftests are rendered at an 800x600 CSS-px viewport (set per test via
        # WebDriver Set Window Rect). At the device's native DPR (e.g. 3.25),
        # 800x600 CSS px would exceed the physical panel, so run reftest
        # sessions at DPR 1, like the desktop headless product.
        binary_args.append("--device-pixel-ratio=1")
    return {
        "server_config": config,
        "webdriver_port": kwargs["webdriver_port"],
        "binary_args": binary_args,
        # The bundle name is overridable via the shared wptrunner `--package-name`
        # flag; the ability and the `hdc` binary are fixed (EntryAbility, `hdc` on
        # PATH).
        "package_name": kwargs.get("package_name") or DEFAULT_PACKAGE,
        "ability_name": DEFAULT_ABILITY,
        "hdc_binary": "hdc",
        "device_serial": kwargs.get("device_serial"),
        "user_stylesheets": kwargs.get("user_stylesheets"),
    }


def executor_kwargs(logger, test_type, test_environment, run_info_data, **kwargs):
    rv = base_executor_kwargs(test_type, test_environment, run_info_data, **kwargs)
    rv['capabilities'] = {}
    return rv


def env_extras(**kwargs):
    return []


def env_options():
    return {"server_host": "127.0.0.1", "supports_debugger": False}


def update_properties():
    return (["debug", "os", "processor", "subsuite"], {"os": ["version"], "processor": ["bits"]})


class HilogRunner:
    """Streams the device's ``hilog`` into the structured logger for the
    duration of a test run, so on-device Servo output — including Rust panic
    output, which servoshell redirects into hilog — is visible when a test
    produces an unexpected result.

    Analogous to chrome_android's ``LogcatRunner``, but reading OHOS ``hilog``
    over hdc. hilog is system-wide and very chatty, so we filter to Servo's
    domain at the source with ``hilog -D`` (see ``SERVO_HILOG_DOMAIN``) unless
    ``SERVO_OHOS_WPT_HILOG_ALL`` is set.
    """

    def __init__(self, logger, browser: "ServoOhosBrowser"):
        self.logger = logger
        self.browser = browser
        self._proc: subprocess.Popen | None = None
        self._thread: threading.Thread | None = None
        self._keep_all = bool(os.environ.get("SERVO_OHOS_WPT_HILOG_ALL"))

    @property
    def pid(self) -> int | None:
        return self._proc.pid if self._proc is not None else None

    def start(self) -> None:
        # Clear the buffer so we don't replay logs from before this run.
        self.browser._hdc_shell("hilog -r", check=False)
        # Bare `hilog` follows the buffer (like `logcat`), streaming until the
        # process is terminated in stop(). Filter to Servo's domain at the
        # source unless asked to capture everything — far cheaper than piping
        # the whole system log over hdc.
        hilog_args = ["hilog"] if self._keep_all else ["hilog", "-D", SERVO_HILOG_DOMAIN]
        cmd = self.browser._hdc_base_cmd() + ["shell"] + hilog_args
        self.logger.debug("hilog: " + " ".join(cmd))
        try:
            self._proc = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
            )
        except OSError as e:
            self.logger.warning(f"Failed to start hilog capture: {e}")
            return
        self._thread = threading.Thread(target=self._pump, name="hilog", daemon=True)
        self._thread.start()

    def _pump(self) -> None:
        proc = self._proc
        if proc is None or proc.stdout is None:
            return
        try:
            for line in proc.stdout:
                line = line.rstrip("\n")
                if line:
                    self.logger.process_output(proc.pid, line, command="hilog")
        except Exception:
            # The pipe goes away when we terminate the process; don't let the
            # reader thread raise during teardown.
            pass

    def stop(self) -> None:
        proc, self._proc = self._proc, None
        if proc is not None and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        if self._thread is not None:
            self._thread.join(timeout=5)
            self._thread = None


class ServoOhosBrowser(Browser):
    """wptrunner Browser implementation that drives Servo running on an
    OpenHarmony device.

    The on-device Servo app exposes a WebDriver server on a configurable port
    (passed via `aa start --ps=--webdriver`). We bridge that port to the host
    with `hdc fport`, and reverse-forward every WPT server port with
    `hdc rport` so the device can reach the WPT server on the host.
    """

    init_timeout: float = 120
    shutdown_retry_attempts = 3

    def __init__(self,
                 logger,
                 *,
                 server_config,
                 webdriver_port: int,
                 binary_args=None,
                 package_name: str = DEFAULT_PACKAGE,
                 ability_name: str = DEFAULT_ABILITY,
                 hdc_binary: str = "hdc",
                 device_serial=None,
                 user_stylesheets=None,
                 **kwargs):
        super().__init__(logger, **kwargs)
        self.server_config = server_config
        self.webdriver_port = int(webdriver_port)
        self.binary_args = list(binary_args) if binary_args else []
        self.package_name = package_name
        self.ability_name = ability_name
        self.hdc_binary = hdc_binary
        # `--device-serial` is `action="append"`, so wptrunner hands us a list
        # (one serial per parallel worker, indexed by manager_number). It's
        # empty when unspecified, in which case we leave the serial as None and
        # let hdc target the single connected device.
        self.device_serial = device_serial[self.manager_number] if device_serial else None
        self.user_stylesheets = user_stylesheets or []

        self.host = "127.0.0.1"
        # Local-side port of the `hdc fport` bridge. Matches the device port to
        # avoid surprise; could be made dynamic if collisions become an issue.
        self.port = self.webdriver_port
        self.base_path = "/"

        # Local temp files; populated in setup() and removed in cleanup().
        self._hosts_path_local: str | None = None
        self._forwarded_ports: list[int] = []
        # WPT server ports reverse-forwarded in setup(), torn down in cleanup().
        self._reversed_ports: list[int] = []
        # Device-side paths of the user stylesheets pushed in setup().
        self._device_user_stylesheets: list[str] = []
        # Fault reports already on the device (snapshotted in setup()) or
        # already reported by check_crash(); everything else is attributed to
        # the current test. None until the setup() snapshot has been taken.
        self._seen_faultlogs: set[str] | None = None
        # Set when the fault log directory turns out to be unreadable (e.g. a
        # device without developer options), so we only warn about it once.
        self._faultlog_unavailable = False
        # True whenever the app being down is not a crash: before the first
        # start() and after we force-stopped it ourselves in stop().
        self._expect_dead = True
        self._screen_wakelock_held = False
        # Register the wakelock release once, here, rather than in setup():
        # repeated setup() calls (e.g. a browser restart after a crash) would
        # otherwise stack a fresh atexit handler each time. This drops the lock
        # on abnormal exits that bypass cleanup() (e.g. Ctrl-C racing with
        # subprocess teardown). It's guarded by `_screen_wakelock_held`, so
        # registering before the lock is acquired — and any double call in a
        # clean run — is harmless.
        atexit.register(self._release_screen_wakelock)
        self.hilog_runner = HilogRunner(self.logger, self)

    # --- hdc helpers -------------------------------------------------------

    def _hdc_base_cmd(self) -> list[str]:
        """The hdc command prefix (binary + device selector), without a
        subcommand. Used both by `_hdc` and by the hilog stream reader."""
        cmd = [self.hdc_binary]
        if self.device_serial:
            cmd += ["-t", self.device_serial]
        return cmd

    def _hdc(self, *args, check=True, capture=True):
        cmd = self._hdc_base_cmd() + list(args)
        self.logger.debug("hdc: " + " ".join(cmd))
        result = subprocess.run(
            cmd,
            capture_output=capture,
            text=True,
            timeout=60,
            check=False,
        )
        if check and result.returncode != 0:
            raise RuntimeError(
                f"hdc command failed ({' '.join(cmd)}): "
                f"stdout={result.stdout!r} stderr={result.stderr!r}"
            )
        return result

    def _hdc_shell(self, command: str, check=True):
        return self._hdc("shell", command, check=check)

    def _push(self, local_path: str, device_path: str):
        """Push a file to the Servo app sandbox.

        Uses ``hdc file send -b org.servo.servo`` so the file lands with the
        bundle uid/gid and SELinux labels — the running app cannot read files
        landed in `/data/local/tmp/`. Requires host hdc >= 3.1.0e and a
        debug-signed bundle that has been started at least once.
        """
        self._hdc("file", "send", "-b", self.package_name, local_path, device_path)

    def _wptserve_ports(self) -> list[int]:
        """Every TCP port the WPT servers listen on, across all schemes
        (http, https, ws, wss, h2, …).

        Derived from `server_config.ports` directly — like firefox_android's
        `config.ports` loop — rather than a module-level global populated from
        a side channel. Some schemes may have `None` entries when disabled, so
        those are filtered out."""
        ports: set[int] = set()
        for port_list in self.server_config.ports.values():
            ports.update(port for port in port_list if port is not None)
        return sorted(ports)

    # --- Browser lifecycle -------------------------------------------------

    def setup(self) -> None:
        """Push static artifacts and configure hdc port forwarding.

        Called once per browser instance, before any test runs.
        """
        # Wait for the device to become available before issuing any other hdc
        # command — the analogue of chrome_android's `adb wait-for-device`.
        # Returns promptly when a device is already connected; bounded by
        # `_hdc`'s timeout if the device is missing, giving a clear early
        # failure instead of a confusing one mid-setup.
        self._hdc("wait")

        # Keep the screen on for the duration of the test run. Without this
        # the device may dim/lock mid-test (the screen-off timeout is
        # user-configurable on OHOS and can be as short as ~10s). Skip
        # `power-shell wakeup` (~600 ms on some devices) when the screen is
        # already on — the lock alone is enough.
        screenlock_status = self._hdc_shell(
            'hidumper -s ScreenlockService -a "-all"', check=False
        ).stdout
        if "screenState" not in screenlock_status or " true" not in screenlock_status.replace("\t", " "):
            self._hdc_shell("power-shell wakeup", check=False)
        self._hdc_shell('hidumper -s PowerManagerService -a "-t"', check=False)
        self._screen_wakelock_held = True

        # The bundle's per-app sandbox needs to be realized on disk once after
        # install before `hdc file send -b` can target it. Skip the
        # `aa start` + `aa force-stop` "realize" dance (~800 ms) when the
        # sandbox already exists from a previous run.
        sandbox_check = self._hdc_shell(
            f"test -d /data/app/el2/100/base/{self.package_name}/haps/{HAP_MODULE}/files"
            " && echo ok",
            check=False,
        ).stdout
        if "ok" not in sandbox_check:
            self._hdc_shell(f"aa start -a {self.ability_name} -b {self.package_name}", check=False)
            self._hdc_shell(f"aa force-stop {self.package_name}", check=False)

        # Hosts file: map every WPT virtual host to 127.0.0.1 so the device
        # resolves them locally (and the request reaches the host via the
        # `hdc rport` reverse forwarding).
        hosts_fd, self._hosts_path_local = tempfile.mkstemp(prefix="servo-wpt-hosts-")
        with os.fdopen(hosts_fd, "w") as f:
            f.write(make_hosts_file(self.server_config, "127.0.0.1"))
        self._push(self._hosts_path_local, f"{DEVICE_FILES_DIR}/wpt-hosts")

        # CA cert for the WPT-issued certificate.
        ca_cert_path = self.server_config.ssl_config["ca_cert_path"]
        if ca_cert_path:
            self._push(ca_cert_path, f"{DEVICE_FILES_DIR}/wpt-cacert.pem")

        # User stylesheets live on the host; push each into the sandbox and
        # remember the device-side path so `_aa_start_args` can point servoshell
        # at it. servoshell reads the file at argument-parse time, so it has to
        # exist on the device before launch.
        self._device_user_stylesheets = []
        for index, stylesheet in enumerate(self.user_stylesheets):
            device_path = f"{DEVICE_FILES_DIR}/wpt-user-stylesheet-{index}.css"
            self._push(stylesheet, device_path)
            self._device_user_stylesheets.append(device_path)

        # Forward the device WebDriver port to the host.
        self._hdc("fport", f"tcp:{self.port}", f"tcp:{self.webdriver_port}")
        self._forwarded_ports.append(self.port)

        # Reverse-forward every WPT server port so the device can talk to the
        # host's WPT server.
        self._reversed_ports = self._wptserve_ports()
        for port in self._reversed_ports:
            self._hdc("rport", f"tcp:{port}", f"tcp:{port}")

        # Snapshot the fault reports that already exist on the device so
        # check_crash() only ever reports ones produced by this run.
        self._seen_faultlogs = self._list_faultlogs() or set()

        # Start streaming the device log so on-device Servo output (and Rust
        # panics, which servoshell redirects into hilog) is captured for the
        # whole run.
        self.hilog_runner.start()

    def _aa_start_args(self) -> list[str]:
        """Build the argument list for `aa start` to launch Servo with
        WebDriver enabled, pointing at our pushed hosts/cert files.

        Every servoshell flag is passed via ``--psn=--<key>=<value>`` (single
        token, ``=``-glued). The two-token ``--ps=--<key> <value>`` form looks
        equivalent but suffers from key-deduplication in the OHOS want
        parameters map (last writer wins), which silently drops repeated
        flags like ``--pref``.
        """

        servoshell_flags: list[str] = [
            # Surface the first thread panic as an immediate exit, instead of
            # an about:failure page that can later trigger downstream crashes
            # (e.g. in the vsync renderer thread) and mask the real failure.
            "--hard-fail",
            f"--webdriver={self.webdriver_port}",
        ]
        # The on-device build must include the `--host-file` flag for WPT
        # tests that use virtual hosts to resolve correctly. Disabled via env
        # var while debugging the rest of the stack on older device builds.
        if not os.environ.get("SERVO_OHOS_WPT_SKIP_HOST_FILE"):
            servoshell_flags.append(f"--host-file={DEVICE_FILES_DIR}/wpt-hosts")
        if self.server_config.ssl_config.get("ca_cert_path"):
            servoshell_flags.append(f"--certificate-path={DEVICE_FILES_DIR}/wpt-cacert.pem")

        # User stylesheets, pushed to the device in setup(). Passed in the
        # `=`-glued single-token form so they survive the --psn= mechanism; the
        # desktop product's two-token `--user-stylesheet <path>` form would be
        # split and silently mangled (see binary_args below).
        for device_path in self._device_user_stylesheets:
            servoshell_flags.append(f"--user-stylesheet={device_path}")

        # Pass through any extra binary_args. Only the single-token forms map
        # cleanly onto --psn=: "--name=value" (value glued) or a bare "--name".
        # A two-token "--name value" pair arrives here as two list entries, and
        # the lone value ("value") can't be matched back to its flag — forwarding
        # it would silently corrupt the launch — so reject anything that isn't a
        # "--"-prefixed token instead of dropping it on the floor.
        for arg in self.binary_args:
            if not arg.startswith("--"):
                raise ValueError(
                    f"servoshell argument {arg!r} cannot be forwarded to the OHOS device: "
                    "flags must use the single-token '--name=value' or bare '--name' form "
                    "(two-token '--name value' flags are unsupported)."
                )
            servoshell_flags.append(arg)

        cmd = [
            "aa", "start",
            "-a", self.ability_name,
            "-b", self.package_name,
        ]
        for flag in servoshell_flags:
            cmd.append(f"--psn={flag}")
        return cmd

    def start(self, group_metadata, **kwargs) -> None:
        # Make sure no stale instance is running.
        self._hdc_shell(f"aa force-stop {self.package_name}", check=False)

        cmd = " ".join(self._aa_start_args())
        self.logger.info(f"Launching OHOS Servo: {cmd}")
        self._hdc_shell(cmd)

        # HTTP-level readiness probe (`GET /status`). The wptrunner default
        # `wait_for_service` is TCP-only, which on OHOS is fooled by the
        # `hdc fport` bridge: the host-side accept completes immediately
        # when the hdc daemon listens, regardless of whether the device-side
        # `127.0.0.1:7000` is bound yet. A TCP-only probe therefore returns
        # "ready" before Servo has actually finished its WebDriver-server
        # setup, and the next `POST /session` races against device-side
        # bind/accept — observed failure mode is `Connection reset by peer`
        # ~150 ms after the probe succeeds. HTTP `/status` requires end-to-
        # end success, which the bridge can't fake.
        deadline = time.time() + self.init_timeout
        while time.time() < deadline:
            try:
                response = requests.get(
                    f"http://{self.host}:{self.port}/status", timeout=1
                )
                if response.status_code == 200:
                    break
            except requests.exceptions.RequestException:
                pass
            time.sleep(0.05)
        else:
            self.logger.error(
                f"OHOS WebDriver did not answer /status at {self.host}:{self.port} "
                f"within {self.init_timeout}s"
            )
            raise OSError("WebDriver server did not become ready in time")

        # From here on, the app disappearing means it crashed.
        self._expect_dead = False

    def stop(self, force: bool = False) -> bool:
        self._expect_dead = True
        # Force-stop the app on the device. `aa force-stop` returns quickly
        # but the kernel may take a brief moment to release the bound
        # WebDriver port; wait briefly so the next `start()` doesn't race
        # against a port that's still in TIME_WAIT.
        self._hdc_shell(f"aa force-stop {self.package_name}", check=False)
        # Poll for the port becoming unreachable; the previous Servo's
        # WebDriver listener should be gone by the time `is_alive()` returns
        # False. Bound to ~500 ms to avoid stalling the test loop. Use a short
        # per-probe timeout so a single sluggish check can't overrun that
        # budget — the default is_alive() timeout is sized for crash detection
        # during a run, not for this tight teardown loop.
        deadline = time.time() + 0.5
        while time.time() < deadline and self.is_alive(timeout=0.1):
            time.sleep(0.05)
        return True

    @property
    def pid(self):
        # There is no host-side browser process. Report the hilog streamer's
        # pid instead: it is the process HilogRunner logs the device output
        # under, and wptrunner stores this value as `browser_pid` in each test
        # result, which is how Servo's formatter attributes that output (panic
        # messages, and the fault reports check_crash() logs under the same
        # pid) to a crashed test.
        return self.hilog_runner.pid

    # --- Crash detection -----------------------------------------------------

    def _list_faultlogs(self) -> set[str] | None:
        """The names of this app's fault reports currently on the device, or
        None if the fault log directory cannot be listed."""
        if self._faultlog_unavailable:
            return None
        # hdc shell does not reliably propagate exit codes, so detect failure
        # from the output instead.
        output = self._hdc_shell(f"ls -1 {FAULTLOG_DIR}", check=False).stdout or ""
        if "denied" in output or "No such file or directory" in output:
            self._faultlog_unavailable = True
            self.logger.warning(
                f"Cannot list {FAULTLOG_DIR} ({output.strip()!r}); "
                "on-device crash reports will not be collected."
            )
            return None
        prefixes = tuple(f"{prefix}-{self.package_name}" for prefix in FAULTLOG_PREFIXES)
        return {name for name in output.split() if name.startswith(prefixes)}

    def _read_faultlog(self, name: str) -> str:
        # TODO: Symbolize the libservoshell.so frames here on the host. The
        # deployed .so is stripped, so backtrace lines only show
        # "#NN pc <offset> .../libservoshell.so(<build-id>)", but <offset> is
        # module-relative and the build tree has the unstripped binary
        # (target/openharmony/<triple>/<profile>/libservoshell.so), so
        # `llvm-symbolizer --obj=<unstripped .so> <offset>` resolves function
        # and file:line. Needs the artifact path plumbed in via browser_kwargs
        # (the product has no binary path today) and the build-id from the
        # report checked against the local .so before trusting the result.
        content = self._hdc_shell(f"cat {FAULTLOG_DIR}/{name}", check=False).stdout or ""
        content = content.replace("\r\n", "\n")
        # Keep the report header and the fault thread's backtrace; drop the
        # bulky per-thread/register/maps dumps that follow.
        marker_index = content.find(FAULTLOG_TRUNCATE_MARKER)
        end = marker_index if marker_index != -1 else FAULTLOG_MAX_CHARS
        if len(content) > end:
            content = (
                f"{content[:end].rstrip()}\n"
                f"[... truncated; full report at {FAULTLOG_DIR}/{name} on the device]"
            )
        return content

    def _report_new_faultlogs(self, test: str | None) -> bool:
        """Log any fault reports that appeared since the last check and return
        whether there were any."""
        current = self._list_faultlogs()
        if current is None:
            return False
        if self._seen_faultlogs is None:
            # No setup() snapshot to compare against; treat everything that is
            # there now as pre-existing rather than misattribute old reports.
            self._seen_faultlogs = current
            return False
        new_reports = sorted(current - self._seen_faultlogs)
        self._seen_faultlogs |= current
        for name in new_reports:
            content = self._read_faultlog(name)
            reason = next(
                (line for line in content.splitlines() if line.startswith("Reason:")), ""
            )
            self.logger.error(f"Detected on-device crash report {name} {reason}".rstrip())
            if self.pid is not None:
                # Log under the hilog streamer's pid (== self.pid, see above)
                # so the report is attached to this test's output.
                self.logger.process_output(self.pid, content, command=f"faultlogger:{name}")
            else:
                self.logger.info(content)
        return bool(new_reports)

    def check_crash(self, process: int, test: str) -> bool:
        # Fast path: Servo runs as a single on-device process, so while the app
        # answers /status it cannot have crashed and we skip the hdc round
        # trips. This runs after every test; keep it cheap.
        if self.is_alive():
            return False

        if self._expect_dead:
            # We took the app down ourselves (or never started it); only report
            # a crash if a new fault report shows up.
            return self._report_new_faultlogs(test)

        # The app died under us. Report it as a crash, giving DFX some time to
        # finish writing the fault report so the backtrace lands in the log.
        deadline = time.time() + FAULTLOG_WRITE_DEADLINE
        while True:
            if self._report_new_faultlogs(test):
                return True
            if time.time() > deadline:
                break
            time.sleep(FAULTLOG_POLL_INTERVAL)

        if self.is_alive():
            # The earlier probe was a false alarm (e.g. the WebDriver server
            # was momentarily unresponsive); don't call it a crash.
            return False
        self.logger.warning(
            "Servo died on the device without leaving a fault report; "
            "see the hilog output above for panic messages."
        )
        return True

    def is_alive(self, timeout: float = 3) -> bool:
        try:
            response = requests.get(f"http://{self.host}:{self.port}/status", timeout=timeout)
            return response.status_code == 200
        except requests.exceptions.RequestException:
            return False

    def cleanup(self) -> None:
        # Stop the log stream before tearing down the connection it rides on.
        self.hilog_runner.stop()

        # Best-effort: tear down hdc forwardings and remove local artifacts.
        # Removal of both `fport` and `rport` entries goes through `fport rm`
        # in hdc — `rport rm` is rejected.
        for port in self._reversed_ports:
            self._hdc("fport", "rm", f"tcp:{port}", f"tcp:{port}", check=False)
        self._reversed_ports.clear()

        for port in self._forwarded_ports:
            self._hdc("fport", "rm", f"tcp:{port}", f"tcp:{self.webdriver_port}", check=False)
        self._forwarded_ports.clear()

        if self._hosts_path_local and os.path.exists(self._hosts_path_local):
            try:
                os.remove(self._hosts_path_local)
            except OSError:
                pass
            self._hosts_path_local = None

        # Release the SCREEN wakelock so the device can sleep again. Without
        # this the device stays awake until the next reboot.
        self._release_screen_wakelock()

    def _release_screen_wakelock(self) -> None:
        """Release the SCREEN running lock acquired in setup().

        Idempotent: safe to call from both cleanup() and atexit, even if the
        lock was never acquired."""
        if self._screen_wakelock_held:
            try:
                self._hdc_shell('hidumper -s PowerManagerService -a "-f"', check=False)
            except Exception:
                pass
            self._screen_wakelock_held = False

    def executor_browser(self):
        return ExecutorBrowser, {
            "webdriver_url": f"http://{self.host}:{self.port}{self.base_path}",
            "host": self.host,
            "port": self.port,
            "pac": None,
            "env": os.environ.copy(),
        }
