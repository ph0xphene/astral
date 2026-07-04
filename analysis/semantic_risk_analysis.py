#!/usr/bin/env python3
"""
Semantic vector-space risk analysis for Solidity function chunks.

Primary input: astral_dump.parquet with columns:
vector, function_name, contract_name, file_path, code_content,
referenced_symbols, risk_score.

The script prefers scikit-learn/matplotlib when installed. In a minimal
runtime it falls back to NumPy LOF, NumPy K-Means, PCA and Pillow plotting.
"""

from __future__ import annotations

import argparse
import ast
import json
import math
import os
import re
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np


LOW_LEVEL_PATTERNS = [
    ".call(",
    ".call{",
    ".delegatecall(",
    ".delegatecall{",
    ".staticcall(",
    ".staticcall{",
    "selfdestruct(",
    "assembly",
    "sstore",
    "sload",
    "mload",
    "mstore",
    "create2",
    "returndatacopy",
]


@dataclass
class Row:
    idx: int
    vector: np.ndarray
    function_name: str
    contract_name: str
    file_path: str
    code_content: str
    referenced_symbols: list[str]
    risk_score: float
    kind: str = "function"
    start_line: int = 0
    end_line: int = 0

    @property
    def label(self) -> str:
        return f"{self.contract_name}::{self.function_name}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", default="astral_dump.parquet")
    parser.add_argument("--lance-json", default="analysis/lance_dataset_export.json")
    parser.add_argument("--graph-json", default="astral_graph.json")
    parser.add_argument("--out-dir", default="analysis")
    parser.add_argument("--neighbors", type=int, default=20)
    parser.add_argument("--clusters", type=int, default=0)
    parser.add_argument("--top", type=int, default=10)
    return parser.parse_args()


def parse_symbols(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, list):
        return [str(v) for v in value if v is not None]
    if isinstance(value, np.ndarray):
        return [str(v) for v in value.tolist() if v is not None]
    if isinstance(value, str):
        stripped = value.strip()
        if not stripped:
            return []
        try:
            parsed = ast.literal_eval(stripped)
            if isinstance(parsed, (list, tuple, set)):
                return [str(v) for v in parsed if v is not None]
        except Exception:
            pass
        return [s.strip() for s in stripped.split(",") if s.strip()]
    return [str(value)]


def parse_vector(value: Any) -> np.ndarray:
    if isinstance(value, np.ndarray):
        return value.astype(float)
    if isinstance(value, list):
        return np.asarray(value, dtype=float)
    if isinstance(value, str):
        return np.asarray(ast.literal_eval(value), dtype=float)
    return np.asarray(value, dtype=float)


def load_graph_risk(path: Path) -> dict[str, float]:
    if not path.exists():
        return {}
    graph = json.loads(path.read_text())
    return {
        str(node.get("id")): float(node.get("risk_score", 0) or 0)
        for node in graph.get("nodes", [])
    }


def heuristic_risk(code: str, refs: list[str], kind: str) -> float:
    signature = code.split("{", 1)[0]
    score = 0
    if count_low_level_ops(code):
        score += 4
    public_mutating = (
        re.search(r"\b(public|external)\b", signature)
        and not re.search(r"\b(view|pure)\b", signature)
    )
    if public_mutating and refs:
        score += 3
    if kind == "fallback_or_receive":
        score += 2
    if any(ref in {"nonReentrant", "onlyOwner", "onlyRole", "whenNotPaused"} for ref in refs):
        score -= 3
    return float(max(0, min(10, score)))


def load_rows(args: argparse.Namespace) -> tuple[list[Row], str]:
    parquet = Path(args.input)
    graph_risk = load_graph_risk(Path(args.graph_json))
    records: list[dict[str, Any]]
    source: str

    if parquet.exists():
        try:
            import pandas as pd  # type: ignore
        except Exception as exc:
            raise SystemExit(
                "Parquet input exists, but pandas/pyarrow is unavailable. "
                "Install pandas+pyarrow or pass --lance-json."
            ) from exc
        frame = pd.read_parquet(parquet)
        records = frame.to_dict("records")
        source = str(parquet)
    else:
        lance_json = Path(args.lance_json)
        if not lance_json.exists():
            raise SystemExit(
                f"Neither {parquet} nor {lance_json} exists. "
                "Run `cargo run --bin export_lance_analysis -- .lancedb_data solidity_chunks "
                "analysis/lance_dataset_export.json` or provide astral_dump.parquet."
            )
        records = json.loads(lance_json.read_text())
        source = str(lance_json)

    rows: list[Row] = []
    for idx, rec in enumerate(records):
        refs = parse_symbols(rec.get("referenced_symbols"))
        contract = str(rec.get("contract_name", ""))
        function = str(rec.get("function_name", ""))
        code = str(rec.get("code_content", ""))
        kind = str(rec.get("kind", "function"))
        risk_raw = rec.get("risk_score", None)
        label = f"{contract}::{function}"
        risk = (
            float(risk_raw)
            if risk_raw is not None
            else graph_risk.get(label, heuristic_risk(code, refs, kind))
        )
        rows.append(
            Row(
                idx=idx,
                vector=parse_vector(rec["vector"]),
                function_name=function,
                contract_name=contract,
                file_path=str(rec.get("file_path", "")),
                code_content=code,
                referenced_symbols=refs,
                risk_score=risk,
                kind=kind,
                start_line=int(rec.get("start_line", 0) or 0),
                end_line=int(rec.get("end_line", 0) or 0),
            )
        )
    return rows, source


def pairwise_distances(x: np.ndarray) -> np.ndarray:
    xx = np.sum(x * x, axis=1, keepdims=True)
    d2 = np.maximum(xx + xx.T - 2 * x @ x.T, 0.0)
    return np.sqrt(d2)


def lof_scores(x: np.ndarray, n_neighbors: int) -> np.ndarray:
    try:
        from sklearn.neighbors import LocalOutlierFactor  # type: ignore

        k = min(n_neighbors, len(x) - 1)
        lof = LocalOutlierFactor(n_neighbors=k, metric="euclidean")
        lof.fit_predict(x)
        return -lof.negative_outlier_factor_
    except Exception:
        pass

    n = len(x)
    k = min(n_neighbors, n - 1)
    distances = pairwise_distances(x)
    order = np.argsort(distances, axis=1)
    neighbors = order[:, 1 : k + 1]
    k_distance = distances[np.arange(n), order[:, k]]
    reachability = np.maximum(distances[np.arange(n)[:, None], neighbors], k_distance[neighbors])
    lrd = 1.0 / (reachability.mean(axis=1) + 1e-12)
    return (lrd[neighbors].mean(axis=1) / (lrd + 1e-12)).astype(float)


def kmeans_plus_plus(x: np.ndarray, k: int, seed: int = 42) -> np.ndarray:
    rng = np.random.default_rng(seed)
    centers = [x[rng.integers(0, len(x))]]
    while len(centers) < k:
        d2 = np.min([np.sum((x - c) ** 2, axis=1) for c in centers], axis=0)
        probs = d2 / (d2.sum() + 1e-12)
        centers.append(x[rng.choice(len(x), p=probs)])
    return np.vstack(centers)


def kmeans(x: np.ndarray, k: int) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    try:
        from sklearn.cluster import KMeans  # type: ignore

        model = KMeans(n_clusters=k, random_state=42, n_init=20)
        labels = model.fit_predict(x)
        centers = model.cluster_centers_
    except Exception:
        centers = kmeans_plus_plus(x, k)
        labels = np.zeros(len(x), dtype=int)
        for _ in range(100):
            d2 = np.stack([np.sum((x - c) ** 2, axis=1) for c in centers], axis=1)
            new_labels = d2.argmin(axis=1)
            if np.array_equal(new_labels, labels):
                break
            labels = new_labels
            for cidx in range(k):
                members = x[labels == cidx]
                if len(members):
                    centers[cidx] = members.mean(axis=0)
    dist = np.linalg.norm(x - centers[labels], axis=1)
    return labels, centers, dist


def reduce_2d(x: np.ndarray) -> tuple[np.ndarray, str]:
    try:
        from sklearn.manifold import TSNE  # type: ignore

        perplexity = max(5, min(30, (len(x) - 1) // 3))
        emb = TSNE(
            n_components=2,
            random_state=42,
            init="pca",
            learning_rate="auto",
            perplexity=perplexity,
        ).fit_transform(x)
        return emb, "t-SNE"
    except Exception:
        centered = x - x.mean(axis=0, keepdims=True)
        _, _, vt = np.linalg.svd(centered, full_matrices=False)
        return centered @ vt[:2].T, "PCA fallback"


def count_low_level_ops(code: str) -> int:
    lower = code.lower()
    return sum(lower.count(pattern.lower()) for pattern in LOW_LEVEL_PATTERNS)


def line_count(row: Row) -> int:
    if row.end_line and row.start_line and row.end_line >= row.start_line:
        return row.end_line - row.start_line + 1
    return max(1, row.code_content.count("\n") + 1)


def connectivity(rows: list[Row]) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    names = defaultdict(list)
    for row in rows:
        names[row.function_name].append(row.idx)
        names[row.label].append(row.idx)
    out_degree = np.asarray([len(set(row.referenced_symbols)) for row in rows], dtype=float)
    in_degree = np.zeros(len(rows), dtype=float)
    for row in rows:
        for ref in set(row.referenced_symbols):
            head = ref.split("{", 1)[0].split("(", 1)[0].strip()
            method = head.rsplit(".", 1)[-1]
            for target in names.get(head, []) + names.get(method, []):
                if target != row.idx:
                    in_degree[target] += 1
    return out_degree, in_degree, out_degree + in_degree


def zscore(values: np.ndarray) -> np.ndarray:
    return (values - values.mean()) / (values.std() + 1e-12)


def explain_unique(row: Row, cluster_size: int, centroid_z: float, low_ops: int) -> str:
    traits = []
    code = row.code_content
    sig = code.split("{", 1)[0].strip().replace("\n", " ")
    if row.kind != "function":
        traits.append(f"definition kind `{row.kind}`")
    if low_ops:
        op_word = "operation" if low_ops == 1 else "operations"
        traits.append(f"{low_ops} low-level {op_word}")
    rare_refs = [s for s in row.referenced_symbols if "." in s or s.startswith("_")]
    if rare_refs:
        traits.append("rare/external symbols: " + ", ".join(rare_refs[:3]))
    if centroid_z > 1.0:
        traits.append(f"cluster centroid distance z={centroid_z:.2f}")
    if cluster_size <= 3:
        traits.append(f"small cluster (n={cluster_size})")
    if not traits and sig:
        traits.append("signature: " + sig[:120])
    return "; ".join(traits) or "semantically distant from nearest neighbors"


def color_for_risk(risk: float) -> tuple[int, int, int]:
    risk = max(0.0, min(10.0, risk)) / 10.0
    if risk < 0.5:
        t = risk / 0.5
        return (int(54 + 191 * t), int(162 + 57 * t), int(235 - 188 * t))
    t = (risk - 0.5) / 0.5
    return (245, int(219 - 142 * t), int(47 - 6 * t))


def plot_with_pillow(
    coords: np.ndarray,
    rows: list[Row],
    lof_top: set[int],
    red_flags: list[int],
    out_path: Path,
    reducer_name: str,
) -> None:
    from PIL import Image, ImageDraw, ImageFont

    width, height = 1500, 1050
    margin = 90
    img = Image.new("RGB", (width, height), "white")
    draw = ImageDraw.Draw(img)
    font = ImageFont.load_default()
    xy = coords.astype(float)
    mins = xy.min(axis=0)
    maxs = xy.max(axis=0)
    span = np.maximum(maxs - mins, 1e-9)
    px = margin + (xy[:, 0] - mins[0]) / span[0] * (width - 2 * margin)
    py = height - margin - (xy[:, 1] - mins[1]) / span[1] * (height - 2 * margin)

    draw.text((margin, 25), f"Semantic vector risk map ({reducer_name}); color = risk_score", fill=(20, 20, 20), font=font)
    for i, row in enumerate(rows):
        x, y = int(px[i]), int(py[i])
        r = 5 if i not in lof_top else 9
        color = color_for_risk(row.risk_score)
        draw.ellipse((x - r, y - r, x + r, y + r), fill=color, outline=(40, 40, 40) if i in lof_top else None)
        if i in lof_top:
            draw.ellipse((x - 13, y - 13, x + 13, y + 13), outline=(185, 28, 28), width=2)

    for rank, i in enumerate(red_flags[:5], 1):
        x, y = int(px[i]), int(py[i])
        draw.text((x + 10, y - 10), f"{rank}. {rows[i].label}", fill=(10, 10, 10), font=font)

    # Risk legend.
    lx, ly = width - 260, 55
    for step in range(101):
        color = color_for_risk(step / 10)
        draw.line((lx + step * 2, ly, lx + step * 2, ly + 16), fill=color)
    draw.rectangle((lx, ly, lx + 202, ly + 16), outline=(80, 80, 80))
    draw.text((lx, ly + 24), "0                 risk_score                10", fill=(30, 30, 30), font=font)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    img.save(out_path)


def plot_coords(coords: np.ndarray, rows: list[Row], lof_top: set[int], red_flags: list[int], out_path: Path, reducer_name: str) -> None:
    try:
        import matplotlib.pyplot as plt  # type: ignore

        risks = [row.risk_score for row in rows]
        plt.figure(figsize=(15, 10))
        plt.scatter(coords[:, 0], coords[:, 1], c=risks, cmap="turbo", s=36, alpha=0.82)
        top = sorted(lof_top)
        plt.scatter(coords[top, 0], coords[top, 1], facecolors="none", edgecolors="black", s=180, linewidths=1.8, label="LOF outliers")
        for rank, idx in enumerate(red_flags[:5], 1):
            plt.annotate(f"{rank}. {rows[idx].label}", (coords[idx, 0], coords[idx, 1]), fontsize=8)
        plt.colorbar(label="risk_score")
        plt.title(f"Semantic vector risk map ({reducer_name})")
        plt.tight_layout()
        plt.savefig(out_path, dpi=180)
        plt.close()
    except Exception:
        plot_with_pillow(coords, rows, lof_top, red_flags, out_path, reducer_name)


def compact_code_snippet(code: str, limit: int = 320) -> str:
    one_line = re.sub(r"\s+", " ", code).strip()
    return one_line[:limit] + ("..." if len(one_line) > limit else "")


def main() -> None:
    args = parse_args()
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    rows, source = load_rows(args)
    if len(rows) < 3:
        raise SystemExit("Need at least 3 vectors for outlier analysis.")

    vectors = np.vstack([row.vector for row in rows]).astype(float)
    if vectors.ndim != 2:
        raise SystemExit("Vector column did not form a 2D matrix.")
    risks = np.asarray([row.risk_score for row in rows], dtype=float)
    low_ops = np.asarray([count_low_level_ops(row.code_content) for row in rows], dtype=float)
    loc = np.asarray([line_count(row) for row in rows], dtype=float)
    low_density = low_ops / np.maximum(loc, 1)
    out_degree, in_degree, total_degree = connectivity(rows)

    lof = lof_scores(vectors, args.neighbors)
    k = args.clusters or min(10, max(2, round(math.sqrt(len(rows) / 2))))
    k = min(k, len(rows) - 1)
    labels, centers, centroid_dist = kmeans(vectors, k)

    cluster_counts = Counter(labels.tolist())
    cluster_z = np.zeros(len(rows), dtype=float)
    cluster_anomaly_idxs = []
    for cidx in sorted(cluster_counts):
        members = np.where(labels == cidx)[0]
        if len(members) < 4:
            continue
        z = zscore(centroid_dist[members])
        cluster_z[members] = z
        for local in np.argsort(z)[-2:][::-1]:
            if z[local] >= 1.0:
                cluster_anomaly_idxs.append(int(members[local]))

    isolated = total_degree <= np.quantile(total_degree, 0.25)
    high_risk = risks >= 7
    high_low = low_density >= max(np.quantile(low_density, 0.75), 0.01)
    risk_connectivity_idxs = np.where(high_risk & isolated & high_low)[0].tolist()

    composite = (
        2.0 * zscore(lof)
        + 1.5 * zscore(np.maximum(cluster_z, 0))
        + 2.4 * (risks / 10.0)
        + 1.8 * zscore(low_density)
        - 0.8 * zscore(total_degree)
    )
    red_flags = np.argsort(composite)[::-1][:5].astype(int).tolist()
    lof_top = np.argsort(lof)[::-1][: args.top].astype(int).tolist()
    coords, reducer_name = reduce_2d(vectors)
    plot_path = out_dir / "semantic_risk_map.png"
    plot_coords(coords, rows, set(lof_top), red_flags, plot_path, reducer_name)

    dense_cluster_outliers = sorted(
        set(cluster_anomaly_idxs),
        key=lambda i: (cluster_z[i], centroid_dist[i]),
        reverse=True,
    )[: args.top]

    report = {
        "source": source,
        "rows": len(rows),
        "vector_dim": int(vectors.shape[1]),
        "risk_summary": {
            "min": float(risks.min()),
            "median": float(np.median(risks)),
            "mean": float(risks.mean()),
            "max": float(risks.max()),
            "count_risk_ge_7": int((risks >= 7).sum()),
        },
        "connectivity_summary": {
            "min": float(total_degree.min()),
            "median": float(np.median(total_degree)),
            "max": float(total_degree.max()),
        },
        "lof_top": [
            {
                "rank": rank,
                "label": rows[i].label,
                "file_path": rows[i].file_path,
                "lines": [rows[i].start_line, rows[i].end_line],
                "lof": float(lof[i]),
                "risk_score": float(rows[i].risk_score),
                "cluster": int(labels[i]),
                "cluster_size": int(cluster_counts[int(labels[i])]),
                "centroid_z": float(cluster_z[i]),
                "connectivity": float(total_degree[i]),
                "low_level_ops": int(low_ops[i]),
                "explanation": explain_unique(rows[i], cluster_counts[int(labels[i])], cluster_z[i], int(low_ops[i])),
            }
            for rank, i in enumerate(lof_top, 1)
        ],
        "dense_cluster_deviations": [
            {
                "label": rows[i].label,
                "cluster": int(labels[i]),
                "cluster_size": int(cluster_counts[int(labels[i])]),
                "centroid_distance": float(centroid_dist[i]),
                "centroid_z": float(cluster_z[i]),
                "risk_score": float(rows[i].risk_score),
                "low_level_ops": int(low_ops[i]),
            }
            for i in dense_cluster_outliers
        ],
        "risk_vs_connectivity_hits": [
            {
                "label": rows[i].label,
                "risk_score": float(rows[i].risk_score),
                "connectivity": float(total_degree[i]),
                "out_degree": float(out_degree[i]),
                "in_degree": float(in_degree[i]),
                "low_level_density": float(low_density[i]),
                "low_level_ops": int(low_ops[i]),
            }
            for i in risk_connectivity_idxs
        ],
        "red_flags": [
            {
                "rank": rank,
                "label": rows[i].label,
                "file_path": rows[i].file_path,
                "lines": [rows[i].start_line, rows[i].end_line],
                "risk_score": float(rows[i].risk_score),
                "lof": float(lof[i]),
                "cluster": int(labels[i]),
                "cluster_size": int(cluster_counts[int(labels[i])]),
                "centroid_z": float(cluster_z[i]),
                "connectivity": float(total_degree[i]),
                "low_level_ops": int(low_ops[i]),
                "low_level_density": float(low_density[i]),
                "why": explain_unique(rows[i], cluster_counts[int(labels[i])], cluster_z[i], int(low_ops[i])),
                "snippet": compact_code_snippet(rows[i].code_content),
            }
            for rank, i in enumerate(red_flags, 1)
        ],
        "plot": str(plot_path),
        "reducer": reducer_name,
        "clusters": int(k),
    }

    (out_dir / "semantic_risk_report.json").write_text(json.dumps(report, indent=2, ensure_ascii=False))
    write_markdown_report(report, out_dir / "semantic_risk_report.md")
    print(json.dumps(report, indent=2, ensure_ascii=False))


def write_markdown_report(report: dict[str, Any], path: Path) -> None:
    lines = [
        "# Semantic Vector Risk Report",
        "",
        f"Source: `{report['source']}`",
        f"Rows: `{report['rows']}`, vector dimension: `{report['vector_dim']}`, clusters: `{report['clusters']}`, reducer: `{report['reducer']}`",
        f"Plot: `{report['plot']}`",
        "",
        "## Risk Summary",
        "",
        json.dumps(report["risk_summary"], ensure_ascii=False),
        "",
        "## Top LOF Outliers",
        "",
    ]
    for item in report["lof_top"]:
        lines.append(
            f"{item['rank']}. `{item['label']}` LOF={item['lof']:.3f}, "
            f"risk={item['risk_score']:.1f}, cluster={item['cluster']} "
            f"(n={item['cluster_size']}), degree={item['connectivity']:.0f}. "
            f"{item['explanation']}"
        )
    lines.extend(["", "## Dense Cluster Deviations", ""])
    for item in report["dense_cluster_deviations"]:
        lines.append(
            f"- `{item['label']}` cluster={item['cluster']} n={item['cluster_size']} "
            f"centroid_z={item['centroid_z']:.2f} risk={item['risk_score']:.1f}"
        )
    lines.extend(["", "## Risk vs Connectivity Hits", ""])
    if not report["risk_vs_connectivity_hits"]:
        lines.append("No node matched risk>=7 + isolated-degree quartile + high low-level density simultaneously.")
    for item in report["risk_vs_connectivity_hits"]:
        lines.append(
            f"- `{item['label']}` risk={item['risk_score']:.1f}, degree={item['connectivity']:.0f}, "
            f"low_level_density={item['low_level_density']:.3f}"
        )
    lines.extend(["", "## Red Flags", ""])
    for item in report["red_flags"]:
        lines.append(
            f"{item['rank']}. `{item['label']}` risk={item['risk_score']:.1f}, "
            f"LOF={item['lof']:.3f}, centroid_z={item['centroid_z']:.2f}, "
            f"degree={item['connectivity']:.0f}, low_ops={item['low_level_ops']}. {item['why']}"
        )
    path.write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
