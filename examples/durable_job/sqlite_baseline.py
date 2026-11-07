#!/usr/bin/env python3
"""Equivalent single-job durable state machine using Python stdlib SQLite."""
import os
import sqlite3
import sys


def transition(conn: sqlite3.Connection, mode: str) -> tuple[int, int]:
    # Explicit transaction + conditional updates model the same idempotent
    # state transitions as the persistent Nulang actor. No external effects.
    with conn:
        conn.execute("CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY, stage INTEGER NOT NULL, completions INTEGER NOT NULL)")
        conn.execute("INSERT OR IGNORE INTO jobs VALUES (1, 0, 0)")
        if mode == "prepare":
            conn.execute("UPDATE jobs SET stage=1 WHERE id=1 AND stage=0")
        else:
            conn.execute("UPDATE jobs SET stage=2, completions=completions+1 WHERE id=1 AND stage=1")
        return conn.execute("SELECT stage, completions FROM jobs WHERE id=1").fetchone()


def main() -> None:
    mode = os.environ["DEMO_PHASE"]
    if mode not in ("prepare", "resume"):
        raise ValueError("DEMO_PHASE must be prepare or resume")
    store = os.environ["DEMO_BASELINE_DB"]
    with sqlite3.connect(store, timeout=5.0) as conn:
        if mode == "prepare":
            stage, completions = transition(conn, mode)
            print(f"PREPARED={stage}:{completions}", flush=True)
            # Intentionally block; the parent kills us with SIGKILL.
            sys.stdin.readline()
        else:
            with conn:
                conn.execute("CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY, stage INTEGER NOT NULL, completions INTEGER NOT NULL)")
                conn.execute("INSERT OR IGNORE INTO jobs VALUES (1, 0, 0)")
                stage, completions = conn.execute("SELECT stage, completions FROM jobs WHERE id=1").fetchone()
            print(f"RECOVERED={stage}:{completions}", flush=True)
            stage, completions = transition(conn, mode)
            print(f"COMPLETED={stage}:{completions}", flush=True)


if __name__ == "__main__":
    main()
