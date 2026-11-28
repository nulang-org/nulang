#!/usr/bin/env python3
"""Disposable QEMU guest power-cut qualification, without host block devices."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import time
import uuid

MODES = ("record", "connection", "cross", "checkpoint")
ALL_MODES = (*MODES, "fault")
GUEST = r"""#!/bin/bash
set -eu
export PATH=/bin
fail() { printf 'FABRIC_VM_FAIL %s\n' "$*"; while :; do sleep 3600; done; }
trap 'fail "line $LINENO"' ERR
mount -n -t proc proc /proc
mount -n -t sysfs sysfs /sys
mount -n -t devtmpfs devtmpfs /dev
phase= mode= nonce=
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        fabric_phase=*) phase=${arg#*=};;
        fabric_mode=*) mode=${arg#*=};;
        fabric_nonce=*) nonce=${arg#*=};;
    esac
done
device=/dev/vda
if [[ "$mode" == fault && "$phase" == write ]]; then
    mkdir -p /dev/mapper
    dmsetup --noudevsync --noudevrules create fabric-test --table '0 262144 linear /dev/vda 0'
    device=/dev/dm-0
fi
mount -n -t ext4 -o rw "$device" /data
ip link set lo up
args=(--bind 127.0.0.1:6380 --mode sync --journal /data/cache.wal)
case "$mode" in
    record) args+=(--checkpoint-percent 0);;
    connection) args+=(--group-commit 1 --checkpoint-percent 0);;
    cross|fault) args+=(--cross-client-group-commit 1 --checkpoint-percent 0);;
    checkpoint) args+=(--cross-client-group-commit 1 --max-journal-bytes 8192 --checkpoint-percent 20);;
    *) fail "unknown mode";;
esac
/bin/nulang-fabric "${args[@]}" &
server_pid=$!
for attempt in {1..100}; do
    if { exec 3<>/dev/tcp/127.0.0.1/6380; } 2>/dev/null; then break; fi
    sleep .05
done
line() { IFS= read -r -t 10 answer <&"$1" || fail "response timeout"; }
expect() { line "$1"; [[ "$answer" == "$2"$'\r' ]] || fail "unexpected response: $answer"; }
get() {
    printf '*2\r\n$3\r\nGET\r\n$%d\r\n%s\r\n' "${#2}" "$2" >&"$1"
}
if [[ "$phase" == write ]]; then
    printf '*3\r\n$3\r\nSET\r\n$6\r\nstable\r\n$6\r\nanchor\r\n' >&3
    expect 3 '+OK'
    printf '*5\r\n$3\r\nSET\r\n$6\r\nexpiry\r\n$4\r\ngone\r\n$2\r\nPX\r\n$2\r\n10\r\n' >&3
    expect 3 '+OK'
    sleep .1
    get 3 expiry
    expect 3 '$-1'
    exec 4<>/dev/tcp/127.0.0.1/6380
    exec 5<>/dev/tcp/127.0.0.1/6380
    exec 6<>/dev/tcp/127.0.0.1/6380
    for fd in 3 4 5 6; do
        for i in {1..16}; do printf '*2\r\n$4\r\nINCR\r\n$1\r\nk\r\n' >&"$fd"; done
    done
    declare -A seen=()
    for fd in 3 4 5 6; do
        for i in {1..16}; do
            line "$fd"
            [[ "$answer" =~ ^:([0-9]+)$'\r'$ ]] || fail "non-integer INCR reply"
            value=${BASH_REMATCH[1]}
            ((value >= 1 && value <= 64)) || fail "invalid sequence"
            [[ -z "${seen[$value]:-}" ]] || fail "duplicate sequence"
            seen[$value]=1
        done
    done
    get 3 k; expect 3 '$2'; expect 3 '64'
    if [[ "$mode" == checkpoint ]]; then
        IFS= read -r -N 8 magic < /data/cache.wal
        [[ "$magic" == NLCWAL03 ]] || fail "checkpoint not published"
    fi
    if [[ "$mode" == fault ]]; then
        # This mapping exists ONLY inside the isolated guest.
        dmsetup --noudevsync suspend --noflush --nolockfs fabric-test
        dmsetup --noudevsync --noudevrules reload fabric-test --table '0 262144 error'
        dmsetup --noudevsync --noudevrules resume fabric-test
        printf '*3\r\n$3\r\nSET\r\n$7\r\nunclear\r\n$5\r\nvalue\r\n' >&3
        answer=
        if IFS= read -r -t 10 answer <&3; then fail "response after block-device failure"; else
            rc=$?
            [[ "$rc" == 1 && -z "$answer" ]] || fail "expected clean EOF after block error"
        fi
        if wait "$server_pid"; then fail "faulted server exited successfully"; fi
        printf 'FABRIC_VM_BLOCK_ERROR_CONFIRMED\n'
    fi
elif [[ "$phase" == recover ]]; then
    get 3 stable; expect 3 '$6'; expect 3 'anchor'
    get 3 expiry; expect 3 '$-1'
    get 3 k; expect 3 '$2'; expect 3 '64'
    printf '*2\r\n$4\r\nINCR\r\n$1\r\nk\r\n' >&3
    expect 3 ':65'
else
    fail "unknown phase"
fi
printf 'FABRIC_VM_PASS %s %s %s\n' "$nonce" "$phase" "$mode"
while :; do sleep 3600; done
"""


def newc_entry(out, name, mode, data, major=0, minor=0):
    encoded = name.encode() + b"\0"
    fields = (0, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(encoded), 0)
    out.write(b"070701" + b"".join(f"{n:08x}".encode() for n in fields))
    out.write(encoded)
    out.write(b"\0" * (-out.tell() % 4))
    out.write(data)
    out.write(b"\0" * (-out.tell() % 4))


def build_initramfs(binary, target, fault=False):
    files = {}
    utilities = ("bash", "mount", "ip", "sleep", "cat") + (("dmsetup", "mkdir") if fault else ())
    for name in utilities:
        path = shutil.which(name)
        if not path:
            raise RuntimeError(f"required guest utility missing: {name}")
        files[f"bin/{name}"] = Path(path)
    files["bin/nulang-fabric"] = binary
    for source in list(files.values()):
        # Only trusted local executables; ldd must not inspect untrusted binaries.
        output = subprocess.check_output(["ldd", str(source)], text=True)
        if "not found" in output:
            raise RuntimeError(f"missing ELF dependency for {source}")
        for line in output.splitlines():
            match = re.search(r"(?:=>\s+|^\s*)(/[^\s]+)\s+\(", line)
            if match:
                path = Path(match.group(1))
                files[str(path).lstrip("/")] = path
    dirs = {"bin", "dev", "proc", "sys", "data", "tmp"}
    for name in files:
        dirs.update(str(p) for p in Path(name).parents if str(p) != ".")
    with target.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as archive:
        for name in sorted(dirs):
            newc_entry(archive, name, stat.S_IFDIR | 0o755, b"")
        newc_entry(archive, "dev/console", stat.S_IFCHR | 0o600, b"", 5, 1)
        newc_entry(archive, "dev/null", stat.S_IFCHR | 0o666, b"", 1, 3)
        newc_entry(archive, "init", stat.S_IFREG | 0o755, GUEST.encode())
        for name, source in sorted(files.items()):
            newc_entry(archive, name, stat.S_IFREG | 0o755, source.read_bytes())
        newc_entry(archive, "TRAILER!!!", 0, b"")


def vm_command(qemu, kernel, initramfs, disk, phase, mode, nonce, accel):
    meta = disk.lstat() if disk.exists() else None
    if (meta is None or not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1
            or "," in str(disk) or disk.parent.stat().st_mode & 0o077):
        raise ValueError("VM disk must be a singly linked regular file in a private directory")
    if phase not in ("write", "recover") or mode not in ALL_MODES or not re.fullmatch(r"[a-zA-Z0-9]+", nonce):
        raise ValueError("invalid guest phase/mode/nonce")
    if accel not in ("kvm", "tcg"):
        raise ValueError("invalid accelerator")
    return [qemu, "-nodefaults", "-no-reboot", "-no-shutdown", "-display", "none",
            "-serial", "stdio", "-monitor", "none", "-nic", "none",
            "-machine", f"q35,accel={accel}", "-cpu", "host" if accel == "kvm" else "max",
            "-m", "768", "-smp", "1", "-kernel", str(kernel), "-initrd", str(initramfs),
            "-append", f"console=ttyS0 rdinit=/init loglevel=4 panic=-1 fabric_phase={phase} fabric_mode={mode} fabric_nonce={nonce}",
            "-drive", f"file={disk},format=raw,if=virtio,cache=none,aio=threads"]


def marker_seen(output, marker):
    return any(line.rstrip("\r") == marker for line in output.split("\n")[:-1])


def boot_and_cut(command, log, marker, timeout):
    start = time.monotonic()
    with log.open("wb") as output:
        proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
        try:
            while time.monotonic() - start < timeout:
                if log.stat().st_size > 2 * 1024 * 1024:
                    raise RuntimeError("guest log limit exceeded")
                content = log.read_text(errors="replace")
                if marker_seen(content, marker):
                    if marker.endswith(" write fault") and not marker_seen(content, "FABRIC_VM_BLOCK_ERROR_CONFIRMED"):
                        raise RuntimeError("guest did not confirm block error")
                    proc.kill()
                    proc.wait(timeout=10)
                    if proc.returncode != -9:
                        raise RuntimeError("VM did not terminate by SIGKILL")
                    return time.monotonic() - start
                if "FABRIC_VM_FAIL " in content or proc.poll() is not None:
                    break
                time.sleep(.02)
            raise RuntimeError(f"guest did not reach {marker}:\n{log.read_text(errors='replace')[-6000:]}")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=10)



def ext4_needs_recovery(disk):
    with disk.open("rb") as f:
        f.seek(1024)
        superblock = f.read(1024)
    if len(superblock) != 1024 or int.from_bytes(superblock[56:58], "little") != 0xEF53:
        raise RuntimeError("invalid ext4 superblock")
    return bool(int.from_bytes(superblock[96:100], "little") & 0x4)


def write_report(file, report):
    file.seek(0)
    json.dump(report, file, indent=2)
    file.write("\n")
    file.truncate()
    file.flush()


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--fabric", type=Path, required=True)
    p.add_argument("--kernel", type=Path, required=True)
    p.add_argument("--work-dir", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--accel", choices=("kvm", "tcg"), default="kvm")
    p.add_argument("--mode", choices=ALL_MODES, action="append")
    p.add_argument("--rounds", type=int, default=1)
    p.add_argument("--timeout", type=int, default=45)
    args = p.parse_args()
    modes = args.mode or MODES
    if args.rounds < 1 or args.rounds > 20 or args.timeout < 5 or args.timeout > 300:
        p.error("rounds 1..20; timeout 5..300 seconds")
    binary, kernel, work = args.fabric.resolve(), args.kernel.resolve(), args.work_dir.resolve()
    if not binary.is_file() or not kernel.is_file() or not work.is_dir():
        p.error("fabric/kernel must be existing regular files; work directory must exist")
    if shutil.disk_usage(work).free < 512 * 1024 * 1024:
        p.error("work directory needs at least 512 MiB free")
    qemu = shutil.which("qemu-system-x86_64")
    if not qemu or not shutil.which("mke2fs"):
        p.error("QEMU and mke2fs required")
    report = {"scope": "abrupt QEMU termination, host remains running; not physical power loss",
              "binary_sha256": digest(binary), "kernel_sha256": digest(kernel),
              "qemu_version": subprocess.check_output([qemu, "--version"], text=True).splitlines()[0],
              "accelerator": args.accel, "filesystem": "ext4 in fresh 128 MiB sparse raw file",
              "cache": "none", "rounds": args.rounds, "expected_cases": len(modes) * args.rounds,
              "cases": []}
    # Exclusive creation: never overwrite an existing file or follow a
    # caller-supplied output symlink/device. A failed run is visibly incomplete.
    with args.output.open("x") as report_file, tempfile.TemporaryDirectory(prefix="fabric-vm-", dir=work) as tmp:
        write_report(report_file, report)
        root = Path(tmp)
        initrd = root / "initramfs.gz"
        build_initramfs(binary, initrd, "fault" in modes)
        for index in range(args.rounds):
            for mode in modes:
                disk = root / "disk.raw"
                with disk.open("xb") as f:
                    f.truncate(128 * 1024 * 1024)
                subprocess.run(["mke2fs", "-q", "-t", "ext4", "-F", "-m", "0",
                                "-E", "lazy_itable_init=0,lazy_journal_init=0", str(disk)], check=True)
                nonce = uuid.uuid4().hex
                result = {"round": index + 1, "mode": mode, "passed": False}
                for phase in ("write", "recover"):
                    command = vm_command(qemu, kernel, initrd, disk, phase, mode, nonce, args.accel)
                    seconds = boot_and_cut(command, root / f"{phase}.log",
                                           f"FABRIC_VM_PASS {nonce} {phase} {mode}", args.timeout)
                    result[f"{phase}_seconds"] = round(seconds, 3)
                    if phase == "write":
                        result["ext4_needs_recovery"] = ext4_needs_recovery(disk)
                        if not result["ext4_needs_recovery"]:
                            raise RuntimeError("guest did not leave a dirty ext4 journal")
                result["passed"] = True
                report["cases"].append(result)
                write_report(report_file, report)
                disk.unlink()
                print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
