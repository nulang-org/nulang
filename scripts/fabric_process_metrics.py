"""Optional Linux server-process telemetry for RESP benchmarks."""
from dataclasses import dataclass
import os
from pathlib import Path


@dataclass(frozen=True)
class Snapshot:
    cpu_seconds: float
    rss_bytes: int
    peak_rss_bytes: int


def read_snapshot(pid: int, proc_root: Path = Path("/proc"),
                  ticks_per_second: int | None = None) -> Snapshot:
    """Read user + system CPU time and resident memory for one process."""
    if ticks_per_second is None:
        ticks_per_second = os.sysconf("SC_CLK_TCK")
    if pid <= 0 or ticks_per_second <= 0:
        raise ValueError("invalid PID or CPU tick rate")
    directory = proc_root / str(pid)
    # comm (field 2) can contain whitespace and closing parentheses.
    fields = (directory / "stat").read_text().rsplit(")", 1)[1].split()
    cpu = (int(fields[11]) + int(fields[12])) / ticks_per_second
    memory = {}
    for line in (directory / "status").read_text().splitlines():
        name, _, rest = line.partition(":")
        if name in ("VmRSS", "VmHWM"):
            count, unit = rest.split()
            if unit != "kB":
                raise ValueError(f"unexpected {name} unit: {unit}")
            memory[name] = int(count) * 1024
    return Snapshot(cpu, memory["VmRSS"], memory["VmHWM"])


def summarize_process(idle: Snapshot, seeded: Snapshot, start: Snapshot,
                      end: Snapshot, keys: int, commands: int) -> dict:
    if keys <= 0 or commands <= 0 or end.cpu_seconds < start.cpu_seconds:
        raise ValueError("invalid key count, command count or CPU interval")
    return {
        "rss_idle_bytes": idle.rss_bytes,
        "rss_after_preload_bytes": seeded.rss_bytes,
        "rss_preload_delta_bytes_per_key": (seeded.rss_bytes - idle.rss_bytes) / keys,
        "rss_after_workload_bytes": end.rss_bytes,
        "peak_rss_bytes": end.peak_rss_bytes,
        "server_cpu_seconds": end.cpu_seconds - start.cpu_seconds,
        "server_cpu_ns_per_command": (end.cpu_seconds - start.cpu_seconds) * 1e9 / commands,
    }
