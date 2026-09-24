#!/usr/bin/env python3
"""Differential RESP compatibility smoke test for Nulang cache vs Valkey.

The test intentionally covers only Nulang's declared RESP core. It compares the
wire-visible semantics of successful operations and keeps all multi-key
operations in one Redis Cluster hash slot.
"""

from __future__ import annotations

import argparse
import socket
import time
from dataclasses import dataclass
from typing import BinaryIO, Iterable


Reply = tuple[str, object]


def encode_command(parts: Iterable[bytes]) -> bytes:
    values = list(parts)
    out = bytearray(f"*{len(values)}\r\n".encode())
    for value in values:
        out.extend(f"${len(value)}\r\n".encode())
        out.extend(value)
        out.extend(b"\r\n")
    return bytes(out)


def read_reply(reader: BinaryIO) -> Reply:
    marker = reader.read(1)
    if not marker:
        raise RuntimeError("RESP peer closed the connection")

    if marker in (b"+", b"-", b":"):
        line = reader.readline()
        if not line.endswith(b"\r\n"):
            raise RuntimeError("invalid RESP line terminator")
        payload = line[:-2]
        if marker == b"+":
            return ("simple", payload)
        if marker == b"-":
            return ("error", payload)
        return ("integer", int(payload))

    if marker == b"$":
        length_line = reader.readline()
        if not length_line.endswith(b"\r\n"):
            raise RuntimeError("invalid RESP bulk length")
        length = int(length_line[:-2])
        if length == -1:
            return ("bulk", None)
        payload = reader.read(length)
        if len(payload) != length or reader.read(2) != b"\r\n":
            raise RuntimeError("truncated RESP bulk value")
        return ("bulk", payload)

    if marker == b"*":
        length_line = reader.readline()
        if not length_line.endswith(b"\r\n"):
            raise RuntimeError("invalid RESP array length")
        length = int(length_line[:-2])
        if length == -1:
            return ("array", None)
        return ("array", [read_reply(reader) for _ in range(length)])

    raise RuntimeError(f"unsupported RESP marker: {marker!r}")


@dataclass
class RespClient:
    sock: socket.socket
    reader: BinaryIO

    @classmethod
    def connect(cls, host: str, port: int, timeout: float = 10.0) -> "RespClient":
        deadline = time.monotonic() + timeout
        last_error: OSError | None = None
        while time.monotonic() < deadline:
            try:
                sock = socket.create_connection((host, port), timeout=1.0)
                sock.settimeout(5.0)
                return cls(sock=sock, reader=sock.makefile("rb"))
            except OSError as error:
                last_error = error
                time.sleep(0.1)
        raise RuntimeError(f"could not connect to {host}:{port}: {last_error}")

    def command(self, *parts: bytes) -> Reply:
        self.sock.sendall(encode_command(parts))
        return read_reply(self.reader)

    def pipeline(self, commands: list[tuple[bytes, ...]]) -> list[Reply]:
        self.sock.sendall(b"".join(encode_command(command) for command in commands))
        return [read_reply(self.reader) for _ in commands]

    def close(self) -> None:
        self.reader.close()
        self.sock.close()


def parse_endpoint(value: str) -> tuple[str, int]:
    host, separator, raw_port = value.rpartition(":")
    if not separator or not host:
        raise argparse.ArgumentTypeError("endpoint must be HOST:PORT")
    try:
        port = int(raw_port)
    except ValueError as error:
        raise argparse.ArgumentTypeError("endpoint port must be an integer") from error
    return host, port


def assert_equal(label: str, nulang: Reply, valkey: Reply) -> None:
    if nulang != valkey:
        raise AssertionError(f"{label}: Nulang={nulang!r}, Valkey={valkey!r}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--nulang", type=parse_endpoint, default=("127.0.0.1", 6380))
    parser.add_argument("--valkey", type=parse_endpoint, default=("127.0.0.1", 6379))
    args = parser.parse_args()

    nulang = RespClient.connect(*args.nulang)
    valkey = RespClient.connect(*args.valkey)
    try:
        tag = b"{nulang-compat}"
        text_key = b"diff:" + tag + b":text"
        counter_key = b"diff:" + tag + b":counter"
        binary_key = b"diff:" + tag + b":binary"
        ttl_key = b"diff:" + tag + b":ttl"
        multi_a = b"diff:" + tag + b":a"
        multi_b = b"diff:" + tag + b":b"

        cases: list[tuple[str, tuple[bytes, ...]]] = [
            ("PING", (b"PING",)),
            ("SET text", (b"SET", text_key, b"hello")),
            ("GET text", (b"GET", text_key)),
            ("EXISTS text", (b"EXISTS", text_key)),
            ("SET counter", (b"SET", counter_key, b"41")),
            ("INCR counter", (b"INCR", counter_key)),
            ("GET counter", (b"GET", counter_key)),
            ("SET binary", (b"SET", binary_key, b"\x00\xffhello\r\n")),
            ("GET binary", (b"GET", binary_key)),
            ("MSET same-slot", (b"MSET", multi_a, b"A", multi_b, b"B")),
            ("MGET same-slot", (b"MGET", multi_a, multi_b)),
            ("SET PX", (b"SET", ttl_key, b"ttl-value", b"PX", b"5000")),
            ("GET PX value", (b"GET", ttl_key)),
        ]

        for label, command in cases:
            assert_equal(label, nulang.command(*command), valkey.command(*command))

        assert_equal(
            "EXPIRE",
            nulang.command(b"EXPIRE", text_key, b"60"),
            valkey.command(b"EXPIRE", text_key, b"60"),
        )
        nulang_ttl = nulang.command(b"TTL", text_key)
        valkey_ttl = valkey.command(b"TTL", text_key)
        if nulang_ttl[0] != "integer" or valkey_ttl[0] != "integer":
            raise AssertionError(f"TTL type mismatch: Nulang={nulang_ttl!r}, Valkey={valkey_ttl!r}")
        if abs(int(nulang_ttl[1]) - int(valkey_ttl[1])) > 1:
            raise AssertionError(f"TTL value mismatch: Nulang={nulang_ttl!r}, Valkey={valkey_ttl!r}")

        pipeline = [
            (b"PING",),
            (b"GET", counter_key),
            (b"EXISTS", counter_key),
        ]
        assert_equal(
            "pipeline",
            ("pipeline", nulang.pipeline(pipeline)),
            ("pipeline", valkey.pipeline(pipeline)),
        )

        assert_equal(
            "DEL text",
            nulang.command(b"DEL", text_key),
            valkey.command(b"DEL", text_key),
        )
        assert_equal(
            "GET missing",
            nulang.command(b"GET", text_key),
            valkey.command(b"GET", text_key),
        )

        print("Nulang RESP core matches Valkey for differential smoke cases")
        return 0
    finally:
        nulang.close()
        valkey.close()


if __name__ == "__main__":
    raise SystemExit(main())
