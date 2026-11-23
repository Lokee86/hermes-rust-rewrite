"""Median of runs 2..N (run 1 is a warm-up) for each app's results in benchmarks/results/*.jsonl."""

import json
import pathlib
import statistics

results = pathlib.Path(__file__).parent / "results"
for path in sorted(results.glob("*.jsonl")):
    runs = [json.loads(line) for line in path.read_text().splitlines() if line.strip()][1:]
    windows = [r["first_window_s"] for r in runs if r["first_window_s"] > 0]
    print(f"{path.stem}: runs={len(runs)}"
          f" first_window_s={statistics.median(windows):.2f} (shown {len(windows)}/{len(runs)})"
          f" footprint_mib={statistics.median(r['footprint_mib'] for r in runs):.0f}"
          f" idle_cpu_percent={statistics.median(r['idle_cpu_percent'] for r in runs):.1f}"
          f" processes={statistics.median(r['processes'] for r in runs):.0f}")
