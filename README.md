# Astral: Vector-Space Security Analysis for Smart Contracts

**Find semantic anomalies that pattern-based scanners miss.**

Astral is a local analysis toolkit for Solidity codebases that combines
function-level embeddings, graph-aware code context, and unsupervised outlier
detection. Instead of only asking "does this code match a known bug pattern?",
Astral asks a more audit-native question:

> Which functions are semantically unusual for this protocol, and why?

That makes Astral useful for finding suspicious fork deviations, isolated
privileged flows, custom accounting logic, low-level call paths, and other
logic-level risks that often survive classical static analysis.

## Value Proposition

Traditional scanners are excellent at known patterns. Astral is designed for
unknown weirdness.

It embeds Solidity functions into a vector space, analyzes the geometry of that
space with algorithms such as Local Outlier Factor (LOF), and links the results
back to concrete source code, referenced symbols, risk scores, and topology.
The output is not a replacement for Slither, Mythril, Foundry, or manual review.
It is a triage layer that tells an auditor where the codebase is structurally
and semantically least ordinary.

## Astral vs Traditional Smart-Contract Tools

| Dimension | Traditional Static Analysis | Astral |
| --- | --- | --- |
| Primary method | Rules, AST patterns, symbolic checks, known vulnerability classes | Embeddings, vector geometry, graph expansion, anomaly scoring |
| Best at | Reentrancy patterns, unchecked calls, access-control smells, compiler-level issues | Logic anomalies, custom fork changes, isolated risky flows, unusual semantic clusters |
| Failure mode | Misses bugs that do not match a known rule | Surfaces suspicious code that still needs human interpretation |
| Output | Findings mapped to predefined detectors | Ranked audit targets with semantic and structural context |
| Mental model | "Does this look like a known bug?" | "Does this function behave unlike the rest of this system?" |
| Auditor workflow | Confirm or dismiss detector alerts | Investigate high-signal anomalies first |

## Quick Start

```bash
curl -fsSL https://raw.githubusercontent.com/ph0xphene/astral/main/scripts/install.sh | bash

astral ingest ./contracts
astral query "withdrawal flow with low-level calls" 10
astral export-graph
```

The installer clones the latest `main` branch, builds the Rust CLI in release
mode, and installs `astral` to `~/.local/bin/astral`. To install somewhere else:

```bash
curl -fsSL https://raw.githubusercontent.com/ph0xphene/astral/main/scripts/install.sh | bash -s -- --prefix /usr/local
```

Typical local workflow:

```bash
# Build the semantic database from Solidity sources
astral ingest ./contracts

# Retrieve graph-expanded audit context
astral query "withdrawal flow with low-level calls" 10

# Export topology for visual review
astral export-graph
```

Generated analysis artifacts are local by design. Source code, embeddings, and
reports do not need to leave your machine.

## Why Vector Analysis?

Most smart-contract scanners look for symptoms:

- a low-level call before a state update
- an unchecked return value
- a missing access modifier
- a dangerous opcode
- a known arithmetic or approval pattern

Those checks are valuable, but many high-impact audit findings are not obvious
syntax patterns. They are deviations from the protocol's own internal logic.

For example, imagine a DeFi protocol forked from a battle-tested codebase. Most
ERC20 functions, oracle reads, accounting helpers, and bridge adapters cluster
tightly because they share similar structure and semantics. Then one function
inside the accounting cluster sits far from the centroid:

| Signal | Interpretation |
| --- | --- |
| Same cluster as standard accounting helpers | The function appears to belong to a familiar subsystem |
| High distance from cluster centroid | Its implementation diverges from peer functions |
| Low connectivity | Few other functions reference it, so manual reviewers may skip it |
| High low-level-operation density | The divergence includes operationally risky code |
| Elevated `risk_score` | Static metadata also considers it structurally dangerous |

Astral treats that function as a red flag even if no predefined detector fires.
That is the core idea: use vector geometry to find code that is not merely
"bug-shaped", but "project-weird".

## Analysis Pipeline

```text
Solidity source
  -> AST parsing
  -> function-level chunks
  -> referenced symbol extraction
  -> MiniLM embeddings
  -> vector database
  -> LOF / clustering / centroid-distance analysis
  -> ranked audit targets
```

| Stage | What Astral Computes | Why It Matters |
| --- | --- | --- |
| Parsing | Function names, contracts, source ranges, raw code | Keeps findings tied to auditable source locations |
| Symbol extraction | Calls, modifiers, state references, external interactions | Adds structural context to semantic hits |
| Embedding | 384-dimensional function vectors | Captures similarity beyond exact tokens |
| LOF | Local vector-space outliers | Finds functions unlike their semantic neighborhood |
| Clustering | Dense semantic groups | Separates oracle logic, token logic, math, adapters, mocks |
| Cluster deviation | Distance from cluster centroid | Highlights custom changes inside reused/forked mechanisms |
| Risk scoring | Static metadata plus low-level operation density | Prioritizes anomalies with exploit-relevant structure |

## Example: Logical Anomaly, Not Pattern Match

Suppose two functions both contain a low-level call:

| Function | Pattern Scanner View | Astral View |
| --- | --- | --- |
| `SafeExecutor.execute()` | Known guarded executor pattern; low-level call detected | Semantically close to other executor functions; lower anomaly priority |
| `Vault.withdraw()` | Low-level call detected | Also isolated, far from accounting peers, high risk density; higher audit priority |

The difference is context. A pattern scanner sees the same primitive. Astral
asks whether that primitive appears in an expected semantic neighborhood.

## Core Commands

| Command | Purpose |
| --- | --- |
| `astral ingest <dir>` | Parse Solidity files, embed function chunks, and build the local vector store |
| `astral query "<text>" [k]` | Retrieve semantic matches and expand them with dependency context |
| `astral query-json "<text>" [k]` | Return raw KNN hits and distances for retrieval debugging |
| `astral export-graph [out.json]` | Export topology JSON and a self-contained HTML graph viewer |

## Offline LOF Analysis

The installed CLI builds the semantic database and graph. For the full LOF /
cluster-deviation report, run the analysis script from a source checkout:

```bash
git clone https://github.com/ph0xphene/astral.git
cd astral

cargo run --bin export_lance_analysis -- \
  .lancedb_data solidity_chunks analysis/lance_dataset_export.json

python3 analysis/semantic_risk_analysis.py \
  --lance-json analysis/lance_dataset_export.json \
  --graph-json astral_graph.json \
  --out-dir analysis
```

## Output Artifacts

| Artifact | Description |
| --- | --- |
| `semantic_risk_report.json` | Machine-readable anomaly report |
| `semantic_risk_report.md` | Human-readable audit triage report |
| `semantic_risk_map.png` | 2D projection of the embedding space colored by risk |
| `astral_graph.json` | Function/reference topology graph |
| `astral_graph.html` | Interactive local graph viewer |

## What to Investigate First

Astral is most useful when a function has several independent risk signals:

| Signal | Why Auditors Should Care |
| --- | --- |
| High LOF score | The function is locally unusual in vector space |
| High centroid distance | It deviates from the cluster it appears to belong to |
| High `risk_score` | Static structure suggests dangerous behavior |
| Low connectivity | The function may be a hidden edge path |
| Low-level operation density | The code touches calls, delegatecalls, assembly, or raw memory/storage |
| Sensitive symbol references | The function interacts with balances, ownership, or cross-chain endpoints |

The strongest red flags are not merely "high risk" or "high anomaly". They are
where semantic weirdness and exploit-relevant structure overlap.

## Design Principles

- **Local-first**: no hosted vector DB, no required API keys, no source upload.
- **Auditor-first**: every score must point back to concrete source code.
- **Complementary**: Astral augments static analyzers; it does not replace them.
- **Unsupervised by default**: useful even before a labeled vulnerability corpus exists.
- **Protocol-relative**: suspicious means unusual for this codebase, not unusual
  in the abstract.

## Status

Astral is an experimental security research tool. Treat its output as a ranked
review queue, not as a proof of safety or exploitability.

Run it alongside traditional tools, then spend human attention where the vector
space says the system is least normal.
