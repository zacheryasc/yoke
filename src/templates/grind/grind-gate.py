#!/usr/bin/env python3
import json
import os
import subprocess
import sys
from pathlib import Path

BASELINE_PATH = Path(".loop/cstat-baseline.json")
CURRENT_PATH = Path(".loop/cstat-current.json")
ORACLE_CMD = os.environ.get("GRIND_ORACLE", "cargo test")
BENCH_CMD = os.environ.get("GRIND_BENCH", "")
CSTAT_CMD = os.environ.get("GRIND_CSTAT", "cstat --path . --json scorecard")


def emit(text, stream=sys.stdout):
    if text:
        print(text, end="" if text.endswith("\n") else "\n", file=stream)


def fail(message):
    print("GRIND: FAIL")
    print(message)
    sys.exit(1)


def load_json_file(path, label):
    if not path.exists():
        fail(f"{label} missing: {path}")
    text = path.read_text()
    if not text.strip():
        fail(f"{label} is empty: {path}")
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        fail(f"{label} is invalid JSON: {exc}")


def numeric_cost(data, label):
    value = data.get("code_complexity_cost")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        fail(f"{label} missing numeric code_complexity_cost")
    return float(value)


def run_shell(label, command):
    if not command.strip():
        return
    print(f"$ {command}")
    proc = subprocess.run(command, shell=True, text=True, capture_output=True)
    emit(proc.stdout)
    emit(proc.stderr, sys.stderr)
    if proc.returncode != 0:
        fail(f"{label} failed: exit {proc.returncode}")


def run_cstat():
    print(f"$ {CSTAT_CMD}")
    proc = subprocess.run(CSTAT_CMD, shell=True, text=True, capture_output=True)
    CURRENT_PATH.write_text(proc.stdout)
    emit(proc.stdout)
    emit(proc.stderr, sys.stderr)
    if proc.returncode != 0:
        fail(f"cstat scorecard failed: exit {proc.returncode}")
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        fail(f"cstat scorecard output is invalid JSON: {exc}")


def print_top_contributors(data):
    top = data.get("top_contributors")
    if not isinstance(top, list) or not top:
        return
    print("top current contributors:")
    for idx, item in enumerate(top[:10], 1):
        if not isinstance(item, dict):
            print(f"{idx}. {item}")
            continue
        kind = item.get("kind", "?")
        file = item.get("file", "?")
        cost = item.get("cost", "?")
        function = item.get("function")
        if function:
            print(f"{idx}. {kind} {file} function={function} cost={cost}")
        else:
            print(f"{idx}. {kind} {file} cost={cost}")


def main():
    baseline = load_json_file(BASELINE_PATH, "cstat baseline")
    baseline_cost = numeric_cost(baseline, "cstat baseline")

    run_shell("behavior oracle", ORACLE_CMD)
    run_shell("benchmark gate", BENCH_CMD)

    current = run_cstat()
    current_cost = numeric_cost(current, "current cstat scorecard")

    if current_cost < baseline_cost:
        print(
            f"GRIND: PASS cstat scorecard improved: "
            f"baseline={baseline_cost:g} current={current_cost:g}"
        )
        return

    print("GRIND: FAIL")
    print(
        f"cstat scorecard did not improve: "
        f"baseline={baseline_cost:g} current={current_cost:g}"
    )
    print_top_contributors(current)
    sys.exit(1)


if __name__ == "__main__":
    main()
