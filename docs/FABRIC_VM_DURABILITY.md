# Fabric disposable VM durability qualification

This opt-in qualification destroys **only its own disposable QEMU
guest processes**, never a host, existing VM, host block device, or
production journal. It extends process-crash tests to guest-kernel
loss and ext4 journal recovery, with an optional real guest block
device error. It does not certify physical power loss of the host.

## Prerequisites

- Linux x86_64, Python 3.11+, QEMU, mke2fs, and trusted local bash,
  mount, ip, sleep, cat and ldd executables.
- A readable matching kernel with built-in initramfs/gzip, serial
  console, devtmpfs, proc/sysfs, virtio PCI/block, loopback and ext4
  support. For the optional fault case, built-in device mapper with
  linear/error targets, plus dmsetup and mkdir are required.
- KVM access (default), or --accel tcg for software emulation.
- At least 512 MiB free in an existing trusted work directory on a
  disk-backed filesystem, and sufficient memory for one 768 MiB guest.
- A trusted local Linux Fabric executable. The harness copies its ELF
  dependencies using ldd; never point it at an untrusted executable.

Build the locked release candidate where resources permit:

    cargo build --locked --release --no-default-features --features cache-server --bin nulang-fabric

Then run:

    python3 scripts/fabric_vm_durability.py \
      --fabric target/release/nulang-fabric \
      --kernel /boot/vmlinuz-$(uname -r) \
      --work-dir /path/to/local/test-directory \
      --output /tmp/fabric-vm-new-report.json \
      --rounds 3 \
      --mode record --mode connection --mode cross \
      --mode checkpoint --mode fault

The output file must not already exist. Without explicit --mode,
the four power-cut modes run (fault requires explicit opt-in).
Choose a longer --timeout for TCG if needed. A report is complete
only when the number of cases equals expected_cases and every case passed.
Host-side contract tests require no VM:

    python3 -m unittest discover -s scripts/tests -p test_fabric_vm_durability.py

## Qualification protocol

Each case creates a fresh 128 MiB sparse raw file, formats it as
ext4, and boots a minimal initramfs with no network devices, shared
folders, host mounts, or host block devices. The only persistent
guest disk is the private, singly linked regular image. QEMU uses
cache=none (direct host I/O, guest flushes still enabled).

The guest mounts its disk and starts the real standalone Fabric
executable on guest loopback in synchronous mode. It verifies a
persistent string, an expiring key and its expiration, 64 ordered
unique INCR acknowledgements from four connections, and the final
counter. The checkpoint mode additionally requires NLCWAL03 to
be published before proceeding. A nonce-tagged serial marker is
accepted only as an exact complete line. The host kills and waits
for the QEMU process without guest shutdown, unmount or sync.

The host reads the ext4 superblock and requires its NEEDS_RECOVERY
incompatibility bit. It then starts a second guest on the same image,
allows ext4 recovery at mount, verifies the acknowledged string and
counter and absence of the expired key, and requires a subsequent
mutation to succeed. Each fresh image is deleted after its case.
A failed case aborts and leaves a visibly incomplete report.

The optional fault case uses a guest-only linear device-mapper
mapping over that guest's virtual disk. After acknowledged writes,
it suspends the mapping without flushing or locking the mounted
filesystem, reloads an error target, and resumes it. A new SET must
receive EOF without any response, and the server must exit nonzero.
The guest confirms this before the cut; the next boot mounts the
original image directly and verifies the prior acknowledged values.
No device-mapper command executes on the host.

## Interpretation and limitations

This is an actual abrupt guest-kernel loss with filesystem recovery,
not merely SIGKILL of Fabric. It verifies acknowledgement survival
for this specific workload and the fail-stop behavior of a guest
block device that rejects I/O. It does not establish all possible
crash interleavings, physical host power-loss behavior, SSD/controller
volatile-cache guarantees, arbitrary torn sectors, lost/reordered
flushes, or production durability. The host kernel, filesystem and
hardware remain running. The guest fault test does not fill a disk,
and the process file-size test is not ENOSPC. Further qualification
can use a dedicated disposable block model with controlled lost,
reordered and torn writes and a larger seeded crash matrix.

See benchmarks/fabric-vm-durability-20261127.json for the initial
qualified run, including exact binary and kernel SHA-256, QEMU
version, modes, rounds, and dirty ext4 journal evidence. The binary
was built from the exact candidate runtime modules in the optimized
locked focused harness, not the full workspace release profile.
No new runtime dependencies, services or deployment changes.

### Local result

All 15 cases passed (three independent rounds per mode, 30 guest boots).
Each cut left the ext4 NEEDS_RECOVERY flag set. The fault cases verified
no response to the uncertain write and recovery of prior acknowledged
state. The Fedora host used KVM, QEMU 10.2.2 and a Btrfs-backed raw file.
