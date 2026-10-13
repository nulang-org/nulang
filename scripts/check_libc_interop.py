#!/usr/bin/env python3
"""Run independent GNU and musl NUL0 processes in both directions.

Generates short-lived CA and leaf key material; does not commit certificates.
Positive runs exercise NUL0 heartbeat delivery and mutually authenticated TLS.
"""
import argparse
import pathlib
import selectors
import subprocess
import tempfile
from contextlib import contextmanager


def run(cmd, *, timeout=20):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, timeout=timeout)


def generate_certificates(folder: pathlib.Path):
    def openssl(*args):
        run(["openssl", *map(str, args)])

    ca_key = folder / "ca.key"
    ca_cert = folder / "ca.pem"
    openssl("genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256", "-out", ca_key)
    openssl("req", "-new", "-x509", "-sha256", "-key", ca_key, "-out", ca_cert,
            "-days", "1", "-subj", "/CN=Nulang interop test CA",
            "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    ext = folder / "leaf.ext"
    ext.write_text("basicConstraints=critical,CA:FALSE\n"
                   "keyUsage=critical,digitalSignature\n"
                   "extendedKeyUsage=serverAuth,clientAuth\n"
                   "subjectAltName=DNS:localhost\n")
    leaves = {}
    for role in ("gnu", "musl"):
        key, csr, cert = (folder / f"{role}.{extn}" for extn in ("key", "csr", "pem"))
        openssl("genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256", "-out", key)
        openssl("req", "-new", "-sha256", "-key", key, "-out", csr, "-subj", f"/CN={role}")
        openssl("x509", "-req", "-sha256", "-in", csr, "-CA", ca_cert,
                "-CAkey", ca_key, "-CAcreateserial", "-out", cert, "-days", "1",
                "-extfile", ext)
        leaves[role] = [str(ca_cert), str(cert), str(key)]
    return leaves


def parse_ready(line):
    parts = line.strip().split()
    if len(parts) != 3 or parts[0] != "READY":
        raise RuntimeError(f"server did not advertise NUL0 readiness: {line!r}")
    host, sep, port_str = parts[1].rpartition(":")
    if sep != ":" or host != "127.0.0.1" or not port_str.isdecimal() or not 1 <= int(port_str) <= 65535:
        raise RuntimeError(f"invalid localhost bind address in {line!r}")
    if not parts[2].isdecimal() or int(parts[2]) > 2**64 - 1:
        raise RuntimeError(f"invalid NUL0 node id in {line!r}")
    return parts[1], parts[2]


@contextmanager
def live_server(command):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
    try:
        with selectors.DefaultSelector() as sel:
            sel.register(process.stdout, selectors.EVENT_READ)
            if not sel.select(timeout=10):
                raise RuntimeError(f"server did not become ready: {command[0]}")
            ready = process.stdout.readline()
        addr, node_id = parse_ready(ready)
        yield process, addr, node_id
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate()


def check_direction(binaries, server_name, client_name, mode, certs):
    server = binaries[server_name]
    client = binaries[client_name]
    server_args = [str(server), "server", "127.0.0.1:0", mode]
    if mode == "tls":
        server_args.extend(certs[server_name])
    with live_server(server_args) as (process, address, node_id):
        client_args = [str(client), "client", address, node_id, mode]
        if mode == "tls":
            client_args.extend(certs[client_name])
        sent = run(client_args, timeout=20)
        if "SENT heartbeat" not in sent.stdout:
            raise RuntimeError(f"client failed to send NUL0 heartbeat: {sent.stdout!r}")
        out, err = process.communicate(timeout=15)
        if process.returncode != 0 or "PASS heartbeat from" not in out:
            raise RuntimeError(f"server did not receive verified heartbeat: rc={process.returncode}, {out!r}, {err!r}")
    print(f"PASS: {client_name} -> {server_name} ({mode})")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--gnu", type=pathlib.Path, required=True)
    parser.add_argument("--musl", type=pathlib.Path, required=True)
    args = parser.parse_args()
    binaries = {"gnu": args.gnu.resolve(), "musl": args.musl.resolve()}
    for name, binary in binaries.items():
        if not binary.is_file():
            parser.error(f"{name} probe binary not found: {binary}")
    with tempfile.TemporaryDirectory(prefix="nulang-libc-interop-") as tmp:
        certs = generate_certificates(pathlib.Path(tmp))
        for mode in ("plain", "tls"):
            for server_name, client_name in (("gnu", "musl"), ("musl", "gnu")):
                check_direction(binaries, server_name, client_name, mode, certs)


if __name__ == "__main__":
    main()
