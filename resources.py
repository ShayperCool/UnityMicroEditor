"""Sample one Linux process without invoking Unity or reading its arguments.

Call sample() every 0.1-0.25 seconds from the caller's polling loop. CPU is
reported in occupied cores, so cpuPercent can exceed 100 for multiple threads.
GPU memory is NVML's per-process allocation, not whole-device memory usage.
NVML ABI: https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html
"""
from __future__ import annotations

import ctypes
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import time


class ResourceError(Exception):
    pass


def gpu_preflight(display: str | None = None, allow_software: bool = False) -> dict:
    """Inspect an existing X display. Never start Xvfb or change driver settings."""
    selected = display if display is not None else os.environ.get("DISPLAY")
    if not selected:
        raise ResourceError("A graphical benchmark needs an existing DISPLAY.")
    executable = shutil.which("glxinfo")
    if executable is None:
        raise ResourceError("glxinfo is required to verify the graphical benchmark renderer.")
    try:
        result = subprocess.run([executable, "-B"], capture_output=True, text=True,
                                env={**os.environ, "DISPLAY": selected}, timeout=10)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ResourceError(f"Could not inspect the display renderer: {type(error).__name__}") from error
    if result.returncode != 0:
        raise ResourceError(f"glxinfo -B could not inspect DISPLAY {selected}, exit {result.returncode}.")

    values = {}
    for line in result.stdout.splitlines():
        key, separator, value = line.strip().partition(":")
        if separator:
            values[key.lower()] = value.strip()
    renderer = values.get("opengl renderer string", "")
    vendor = values.get("opengl vendor string", "")
    if not renderer or not vendor:
        raise ResourceError("glxinfo did not identify an OpenGL vendor and renderer.")
    markers = ("llvmpipe", "softpipe", "swrast", "software rasterizer", "swiftshader", "gdi generic")
    software = any(marker in renderer.lower() for marker in markers)
    if values.get("accelerated", "").lower() == "no":
        software = True
    if software and not allow_software:
        raise ResourceError(f"Software renderer cannot measure hardware GPU performance: {renderer}")
    return {
        "display": selected, "vendor": vendor, "renderer": renderer,
        "version": values.get("opengl core profile version string", values.get("opengl version string")),
        "directRendering": values.get("direct rendering", "").lower() == "yes",
        "softwareRendering": software, "accelerated": not software,
    }


class _NvmlProcessInfo(ctypes.Structure):
    # nvmlProcessInfo_t, shared by GetGraphicsRunningProcesses_v2 and _v3.
    _fields_ = [("pid", ctypes.c_uint), ("usedGpuMemory", ctypes.c_ulonglong),
                ("gpuInstanceId", ctypes.c_uint), ("computeInstanceId", ctypes.c_uint)]


class _Nvml:
    SUCCESS = 0
    INSUFFICIENT_SIZE = 7
    MEMORY_UNAVAILABLE = (1 << 64) - 1

    def __init__(self):
        self.library = ctypes.CDLL("libnvidia-ml.so.1")
        self.initialized = False
        self.devices = []
        self._bind("nvmlInit_v2", [])
        self._bind("nvmlShutdown", [])
        self._bind("nvmlDeviceGetCount_v2", [ctypes.POINTER(ctypes.c_uint)])
        self._bind("nvmlDeviceGetHandleByIndex_v2", [ctypes.c_uint, ctypes.POINTER(ctypes.c_void_p)])
        self._bind("nvmlDeviceGetName", [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint])
        self.query_name = next((name for name in ("nvmlDeviceGetGraphicsRunningProcesses_v3",
                                                "nvmlDeviceGetGraphicsRunningProcesses_v2")
                                if hasattr(self.library, name)), None)
        if self.query_name is None:
            raise ResourceError("NVML has no compatible graphics process query.")
        self.query = self._bind(self.query_name, [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint),
                                                 ctypes.POINTER(_NvmlProcessInfo)])
        self._check(self.library.nvmlInit_v2(), "nvmlInit_v2")
        self.initialized = True
        try:
            count = ctypes.c_uint()
            self._check(self.library.nvmlDeviceGetCount_v2(ctypes.byref(count)), "nvmlDeviceGetCount_v2")
            if count.value == 0:
                raise ResourceError("NVML enumerated no accessible GPU devices.")
            for index in range(count.value):
                handle = ctypes.c_void_p()
                self._check(self.library.nvmlDeviceGetHandleByIndex_v2(index, ctypes.byref(handle)),
                            "nvmlDeviceGetHandleByIndex_v2")
                name = ctypes.create_string_buffer(256)
                self._check(self.library.nvmlDeviceGetName(handle, name, len(name)), "nvmlDeviceGetName")
                self.devices.append((index, handle, name.value.decode(errors="replace")))
        except Exception:
            self.close()
            raise

    def _bind(self, name, arguments):
        function = getattr(self.library, name)
        function.argtypes = arguments
        function.restype = ctypes.c_int
        return function

    @staticmethod
    def _check(code, name):
        if code != _Nvml.SUCCESS:
            raise ResourceError(f"{name} returned NVML status {code}.")

    def sample(self, pid: int) -> dict:
        devices = []
        for index, handle, name in self.devices:
            capacity = 16
            for _ in range(3):
                records = (_NvmlProcessInfo * capacity)()
                count = ctypes.c_uint(capacity)
                code = self.query(handle, ctypes.byref(count), records)
                if code == self.INSUFFICIENT_SIZE:
                    capacity = max(capacity * 2, count.value + 8)
                    if capacity > 65536:
                        raise ResourceError("NVML graphics process query requested an excessive array.")
                    continue
                self._check(code, self.query_name)
                matches = [record for record in records[:min(count.value, capacity)] if record.pid == pid]
                unavailable = len(matches) > 1 or any(record.usedGpuMemory == self.MEMORY_UNAVAILABLE for record in matches)
                # Keep ambiguous MIG accounting unavailable rather than guessing a total.
                memory = None if unavailable else sum(record.usedGpuMemory for record in matches)
                devices.append({"index": index, "name": name, "pidListed": bool(matches),
                                "memoryMiB": None if memory is None else memory / (1024 * 1024)})
                break
            else:
                raise ResourceError("NVML process list changed repeatedly during sampling.")
        memory = None if any(item["memoryMiB"] is None for item in devices) else sum(item["memoryMiB"] for item in devices)
        return {"status": "available", "memoryMiB": memory, "devices": devices}

    def close(self):
        if self.initialized:
            self.library.nvmlShutdown()
            self.initialized = False


def _read_stat(path: Path, ticks: int, page_bytes: int) -> dict:
    text = path.read_text()
    fields = text[text.rfind(")") + 2:].split()
    return {"identity": int(fields[19]), "state": fields[0],
            "cpuSeconds": (int(fields[11]) + int(fields[12])) / ticks,
            "rssMiB": max(0, int(fields[21])) * page_bytes / (1024 * 1024)}


def _read_memory(process: Path, fallback_rss: float) -> dict:
    try:
        values = {match.group(1): int(match.group(2)) for match in
                  re.finditer(r"^(\w+):\s+(\d+)\s+kB$", (process / "smaps_rollup").read_text(), re.MULTILINE)}
        private = values.get("Private_Clean", 0) + values.get("Private_Dirty", 0) + values.get("Private_Hugetlb", 0)
        return {"rssMiB": values.get("Rss", fallback_rss * 1024) / 1024,
                "pssMiB": values["Pss"] / 1024 if "Pss" in values else None,
                "privateMiB": private / 1024 if "Private_Clean" in values or "Private_Dirty" in values else None,
                "memorySource": "smaps_rollup"}
    except (OSError, ValueError):
        return {"rssMiB": fallback_rss, "pssMiB": None, "privateMiB": None,
                "memorySource": "proc-stat-rss-only"}


class ResourceSampler:
    """Caller-driven sampler. A ready boundary is elapsed seconds from started_at."""

    def __init__(self, pid: int, gpu: bool = True, *, started_at: float | None = None,
                 clock=time.monotonic, proc_root: Path = Path("/proc"), gpu_reader=None):
        if pid <= 0:
            raise ValueError("pid must be positive")
        self.pid = pid
        self.clock = clock
        now = clock()
        self.started_at = now if started_at is None else started_at
        self.started_utc_unix_milliseconds = round((time.time() - (now - self.started_at)) * 1000)
        self.process = Path(proc_root) / str(pid)
        self.ticks = os.sysconf("SC_CLK_TCK")
        self.page_bytes = os.sysconf("SC_PAGE_SIZE")
        self.samples: list[dict] = []
        self.previous = None
        self.gpu = gpu_reader
        self.gpu_error = "disabled" if not gpu else None
        try:
            baseline = _read_stat(self.process / "stat", self.ticks, self.page_bytes)
            self.identity = baseline["identity"]
            self.previous = self.clock() - self.started_at, baseline["cpuSeconds"]
        except (OSError, ValueError, IndexError):
            self.identity = None
        if gpu and self.gpu is None:
            try:
                self.gpu = _Nvml()
            except (OSError, AttributeError, ResourceError) as error:
                self.gpu_error = str(error)

    def sample(self) -> dict:
        elapsed = self.clock() - self.started_at
        sample = {"elapsedSeconds": elapsed, "status": "process-gone"}
        try:
            state = _read_stat(self.process / "stat", self.ticks, self.page_bytes)
            if self.identity is None or state["identity"] != self.identity:
                sample["status"] = "pid-identity-unavailable" if self.identity is None else "pid-reused"
            elif state["state"] not in ("Z", "X"):
                sample.update(status="available", cpuSeconds=state["cpuSeconds"])
                sample.update(_read_memory(self.process, state["rssMiB"]))
                interval = None if self.previous is None else elapsed - self.previous[0]
                delta = None if self.previous is None else state["cpuSeconds"] - self.previous[1]
                cores = delta / interval if interval is not None and interval > 0 and delta >= 0 else None
                sample.update(cpuDeltaSeconds=delta, cpuIntervalSeconds=interval,
                              cpuIntervalStartSeconds=None if self.previous is None else self.previous[0],
                              cpuCores=cores, cpuPercent=None if cores is None else cores * 100)
                self.previous = elapsed, state["cpuSeconds"]
                if self.gpu is not None:
                    try:
                        sample["gpu"] = self.gpu.sample(self.pid)
                    except (OSError, ResourceError) as error:
                        sample["gpu"] = {"status": "unavailable", "reason": str(error), "memoryMiB": None}
                else:
                    sample["gpu"] = {"status": "unavailable", "reason": self.gpu_error, "memoryMiB": None}
                sample["gpuMiB"] = sample["gpu"]["memoryMiB"]
        except (OSError, ValueError, IndexError):
            pass
        self.samples.append(sample)
        return sample

    def summary(self, ready_elapsed: float | None = None, completed_elapsed: float | None = None) -> dict:
        if ready_elapsed is not None and (not math.isfinite(ready_elapsed) or ready_elapsed < 0):
            raise ValueError("ready_elapsed must be finite and nonnegative")
        if completed_elapsed is not None and (not math.isfinite(completed_elapsed) or completed_elapsed < 0):
            raise ValueError("completed_elapsed must be finite and nonnegative")
        if ready_elapsed is not None and completed_elapsed is not None and completed_elapsed < ready_elapsed:
            raise ValueError("completed_elapsed cannot precede ready_elapsed")
        end = max((item["elapsedSeconds"] for item in self.samples), default=0)
        available = [item for item in self.samples if item["status"] == "available"]
        steady_end = end if completed_elapsed is None else completed_elapsed
        return {
            "pid": self.pid, "scope": "single-process", "schemaVersion": 1,
            "startedUtcUnixMilliseconds": self.started_utc_unix_milliseconds,
            "readyElapsedSeconds": ready_elapsed, "completedElapsedSeconds": completed_elapsed,
            "durationSeconds": end,
            "sampleCount": len(self.samples), "availableSampleCount": len(available),
            "overall": self._phase(available, 0, end),
            "startup": self._phase(available, 0, end if ready_elapsed is None else ready_elapsed,
                                   include_end=ready_elapsed is None),
            "steady": {"available": False, "sampleCount": 0, "reason": "ready-not-observed"}
                      if ready_elapsed is None else self._phase(available, ready_elapsed, steady_end,
                                                              include_end=completed_elapsed is None),
            "shutdown": {"available": False, "sampleCount": 0, "reason": "completion-not-observed"}
                        if completed_elapsed is None else self._phase(available, completed_elapsed, end),
            "cpuPercentMeaning": "100 percent equals one occupied CPU core",
            "cpuBoundaryMethod": "CPU deltas crossing a phase boundary are apportioned by interval duration",
            "samples": list(self.samples),
        }

    @staticmethod
    def _phase(samples: list[dict], start: float, end: float, *, include_end: bool = True) -> dict:
        selected = [item for item in samples if start <= item["elapsedSeconds"] and
                    (item["elapsedSeconds"] <= end if include_end else item["elapsedSeconds"] < end)]
        result = {"available": bool(selected), "sampleCount": len(selected),
                  "startElapsedSeconds": start, "endElapsedSeconds": end}
        for source, label in (("rssMiB", "RssMiB"), ("pssMiB", "PssMiB"),
                              ("privateMiB", "PrivateMiB"), ("gpuMiB", "GpuMiB")):
            values = [item[source] for item in selected if item.get(source) is not None]
            result["peak" + label] = max(values) if values else None
            result["mean" + label] = sum(values) / len(values) if values else None
            result["last" + label] = values[-1] if values else None
        cpu, duration = 0.0, 0.0
        cores = []
        for item in samples:
            interval = item.get("cpuIntervalSeconds")
            if not interval or item.get("cpuCores") is None:
                continue
            overlap = max(0.0, min(end, item["elapsedSeconds"]) - max(start, item["cpuIntervalStartSeconds"]))
            if overlap:
                cpu += item["cpuDeltaSeconds"] * overlap / interval
                duration += overlap
                cores.append(item["cpuCores"])
        result.update(cpuObservedSeconds=cpu if duration else None,
                      cpuObservationWallSeconds=duration,
                      meanCpuCores=cpu / duration if duration else None,
                      peakCpuCores=max(cores) if cores else None,
                      meanCpuPercent=100 * cpu / duration if duration else None,
                      peakCpuPercent=100 * max(cores) if cores else None)
        return result

    def close(self):
        if self.gpu is not None and hasattr(self.gpu, "close"):
            self.gpu.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
