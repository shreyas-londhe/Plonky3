#!/usr/bin/env python3
"""Aggregate WHIR cross-field Criterion results + proof sizes into a CSV and plots.

Reads:
  - target/criterion/whir_fields/<op>/<field>/s<sec>/<n>/new/estimates.json  (median ns)
  - .claude/2026-05-31-whir-bench/proof_sizes.csv                            (field,security,log_size,proof_bytes)

Writes (into this script's directory):
  - results.csv          : op,field,security,log_size,median_ns,proof_bytes
  - plot_<op>.png        : median time vs log_size, one line per (field, security)
  - plot_proof_size.png  : proof bytes vs log_size, one line per (field, security)
"""
import csv
import json
import re
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]  # .../Plonky3/.claude/<dir> -> Plonky3
CRIT = REPO / "target" / "criterion"
PROOF_CSV = HERE / "proof_sizes.csv"

# Criterion flattens '/' in group/bench ids to '_':
#   group dir:  whir_fields_<op>
#   bench dir:  <field>_s<sec>_<n>   (field may contain underscores)
BENCH_RE = re.compile(r"^(?P<field>.+)_s(?P<sec>\d+)_(?P<n>\d+)$")

FIELDS = ["baby_bear", "koala_bear", "goldilocks"]
OPS = ["commit", "open", "verify"]


def collect_times():
    """Return {(op, field, sec, n): median_ns}."""
    out = {}
    for op in OPS:
        group = CRIT / f"whir_fields_{op}"
        if not group.exists():
            continue
        for bench_dir in group.iterdir():
            m = BENCH_RE.match(bench_dir.name)
            est = bench_dir / "new" / "estimates.json"
            if not m or not est.exists():
                continue
            data = json.loads(est.read_text())
            median = data.get("median", data.get("mean", {})).get("point_estimate")
            if median is not None:
                out[(op, m["field"], int(m["sec"]), int(m["n"]))] = median
    return out


def collect_proof_sizes():
    """Return {(field, sec, n): bytes}."""
    out = {}
    if not PROOF_CSV.exists():
        return out
    with PROOF_CSV.open() as f:
        for row in csv.DictReader(f):
            out[(row["field"], int(row["security"]), int(row["log_size"]))] = int(
                row["proof_bytes"]
            )
    return out


def write_results(times, proofs):
    rows = []
    keys = sorted(set(times) | {(op, f, s, n) for (f, s, n) in proofs for op in OPS})
    for (op, field, sec, n) in keys:
        rows.append(
            {
                "op": op,
                "field": field,
                "security": sec,
                "log_size": n,
                "median_ns": times.get((op, field, sec, n), ""),
                "proof_bytes": proofs.get((field, sec, n), ""),
            }
        )
    with (HERE / "results.csv").open("w", newline="") as f:
        w = csv.DictWriter(
            f, fieldnames=["op", "field", "security", "log_size", "median_ns", "proof_bytes"]
        )
        w.writeheader()
        w.writerows(rows)
    return rows


def plot(times, proofs):
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    styles = {"baby_bear": "C0", "koala_bear": "C1", "goldilocks": "C2"}
    dash = {100: "--", 128: "-"}

    for op in OPS:
        fig, ax = plt.subplots(figsize=(8, 5))
        plotted = False
        for field in FIELDS:
            for sec in (100, 128):
                pts = sorted(
                    (n, ns / 1e6)
                    for (o, f, s, n), ns in times.items()
                    if o == op and f == field and s == sec
                )
                if not pts:
                    continue
                xs, ys = zip(*pts)
                ax.plot(
                    xs, ys, dash[sec], color=styles[field], marker="o", markersize=3,
                    label=f"{field} s{sec}",
                )
                plotted = True
        if not plotted:
            plt.close(fig)
            continue
        ax.set_yscale("log")
        ax.set_xlabel("log2(polynomial size)")
        ax.set_ylabel(f"{op} median time (ms, log scale)")
        ax.set_title(f"WHIR {op} — Apple M3 Pro")
        ax.grid(True, which="both", alpha=0.3)
        ax.legend(fontsize=8)
        fig.tight_layout()
        fig.savefig(HERE / f"plot_{op}.png", dpi=130)
        plt.close(fig)

    # Proof size
    fig, ax = plt.subplots(figsize=(8, 5))
    for field in FIELDS:
        for sec in (100, 128):
            pts = sorted(
                (n, b / 1024)
                for (f, s, n), b in proofs.items()
                if f == field and s == sec
            )
            if not pts:
                continue
            xs, ys = zip(*pts)
            ax.plot(
                xs, ys, dash[sec], color=styles[field], marker="o", markersize=3,
                label=f"{field} s{sec}",
            )
    ax.set_xlabel("log2(polynomial size)")
    ax.set_ylabel("proof size (KiB)")
    ax.set_title("WHIR proof size")
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=8)
    fig.tight_layout()
    fig.savefig(HERE / "plot_proof_size.png", dpi=130)
    plt.close(fig)


def main():
    times = collect_times()
    proofs = collect_proof_sizes()
    rows = write_results(times, proofs)
    plot(times, proofs)
    print(f"results.csv: {len(rows)} rows ({len(times)} timed cells, {len(proofs)} proof sizes)")


if __name__ == "__main__":
    main()
