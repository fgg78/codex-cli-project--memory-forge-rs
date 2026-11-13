#!/usr/bin/env python3
"""Repair legacy erased Codex turn errors and stale history offsets."""

import argparse
import json
import os
import shutil
import sqlite3
from datetime import datetime
from pathlib import Path


def line_offsets(lines):
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line))
    return offsets


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rollout", type=Path)
    parser.add_argument("--codex-home", type=Path, default=Path.home() / ".codex")
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()

    path = args.rollout.resolve(strict=True)
    codex_home = args.codex_home.resolve(strict=True)
    if codex_home not in path.parents or path.suffix != ".jsonl":
        parser.error("rollout must be a JSONL file inside the selected Codex home")

    original = path.read_bytes()
    lines = original.splitlines(keepends=True)
    rows = [json.loads(line) for line in lines]
    thread_id = rows[0]["payload"]["id"]
    turn_ids = []
    updated_lines = list(lines)
    for index, row in enumerate(rows):
        payload = row.get("payload", {})
        error = payload.get("error")
        if (row.get("type") != "event_msg" or payload.get("type") != "task_complete"
                or not isinstance(error, dict) or error.get("message") != ""
                or error.get("codex_error_info") != "cyber_policy"):
            continue
        turn_ids.append(payload["turn_id"])
        payload["error"] = None
        newline = b"\r\n" if lines[index].endswith(b"\r\n") else b"\n" if lines[index].endswith(b"\n") else b""
        encoded = json.dumps(row, ensure_ascii=False, separators=(",", ":")).encode()
        padding = len(lines[index]) - len(encoded) - len(newline)
        if padding < 0:
            raise ValueError(f"erased row cannot fit within the original byte length: {index}")
        updated_lines[index] = encoded + b" " * padding + newline

    db_path = codex_home / "thread_history_1.sqlite"
    connection = sqlite3.connect(db_path, timeout=5)
    try:
        turns = connection.execute(
            "SELECT turn_id, rollout_ordinal, rollout_end_ordinal, status, error_json "
            "FROM thread_turns WHERE thread_id = ?", (thread_id,)
        ).fetchall()
        found = {turn[0]: turn for turn in turns}
        for turn_id in turn_ids:
            if turn_id not in found or found[turn_id][3] != "failed":
                raise ValueError(f"unexpected turn state: {turn_id}")
            error = json.loads(found[turn_id][4])
            if error.get("message") != "" or error.get("codexErrorInfo") != "cyberPolicy":
                raise ValueError(f"turn projection differs from erased rollout: {turn_id}")

        offsets = line_offsets(updated_lines)
        original_offsets = line_offsets(lines)
        projection = connection.execute(
            "SELECT next_rollout_ordinal, next_rollout_byte_offset "
            "FROM thread_history_projection_state WHERE thread_id = ?", (thread_id,)
        ).fetchone()
        for turn_id, start, end, _, _ in turns:
            if start is not None and end is not None and (start >= len(offsets) or end + 1 >= len(offsets)):
                raise ValueError(f"out-of-range rollout ordinal: {turn_id}")
        if projection and projection[0] >= len(offsets):
            raise ValueError("projection ordinal exceeds rollout length")
        print(f"thread={thread_id} erased_turns={turn_ids} bytes={len(original)}->{offsets[-1]}")
        print(f"projection_offset={projection[1] if projection else None}->{offsets[projection[0]] if projection else None}")
        if not args.apply:
            print("Dry run. Pass --apply to back up and repair the rollout and history database.")
            return
        if not turn_ids:
            print("No legacy erased turn errors found; no changes made.")
            return

        backup_dir = codex_home / "backups" / ("memory-forge-turn-error-" + datetime.now().strftime("%Y%m%d-%H%M%S-%f"))
        backup_dir.mkdir(parents=True, exist_ok=False)
        rollout_backup = backup_dir / path.name
        db_backup = backup_dir / db_path.name
        shutil.copy2(path, rollout_backup)
        backup_connection = sqlite3.connect(db_backup)
        try:
            connection.backup(backup_connection)
        finally:
            backup_connection.close()

        written = []
        try:
            connection.execute("BEGIN IMMEDIATE")
            if path.read_bytes() != original:
                raise ValueError("rollout changed since inspection")
            current_projection = connection.execute(
                "SELECT next_rollout_ordinal, next_rollout_byte_offset "
                "FROM thread_history_projection_state WHERE thread_id = ?", (thread_id,)
            ).fetchone()
            if current_projection != projection:
                raise ValueError("projection changed since inspection")
            for turn_id in turn_ids:
                current = connection.execute(
                    "SELECT status, error_json FROM thread_turns WHERE thread_id = ? AND turn_id = ?",
                    (thread_id, turn_id),
                ).fetchone()
                if current != (found[turn_id][3], found[turn_id][4]):
                    raise ValueError(f"turn changed since inspection: {turn_id}")
                connection.execute(
                    "UPDATE thread_turns SET status = 'completed', error_json = NULL "
                    "WHERE thread_id = ? AND turn_id = ?", (thread_id, turn_id),
                )
            for turn_id, start, end, _, _ in turns:
                if start is not None and end is not None:
                    connection.execute(
                        "UPDATE thread_turns SET rollout_byte_offset = ?, rollout_end_byte_offset = ? "
                        "WHERE thread_id = ? AND turn_id = ?",
                        (offsets[start], offsets[end + 1], thread_id, turn_id),
                    )
            if projection:
                connection.execute(
                    "UPDATE thread_history_projection_state SET next_rollout_byte_offset = ? "
                    "WHERE thread_id = ?", (offsets[projection[0]], thread_id),
                )
            with path.open("r+b", buffering=0) as rollout:
                for index, (before, after) in enumerate(zip(lines, updated_lines)):
                    if before != after:
                        written.append(index)
                        if os.pwrite(rollout.fileno(), after, original_offsets[index]) != len(after):
                            raise OSError(f"short write to rollout row {index}")
                os.fsync(rollout.fileno())
            connection.commit()
        except Exception:
            connection.rollback()
            if written:
                with path.open("r+b", buffering=0) as rollout:
                    for index in written:
                        os.pwrite(rollout.fileno(), lines[index], original_offsets[index])
                    os.fsync(rollout.fileno())
            raise
        print(f"Backup: {backup_dir}")
        print("Repair complete.")
    finally:
        connection.close()


if __name__ == "__main__":
    main()
