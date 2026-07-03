# astral

**Local semantic RAG and topology analysis for Solidity security audits.**

`astral` turns a Solidity codebase into a searchable function graph. It parses
contracts with tree-sitter, embeds function-level chunks with a local
MiniLM model, stores them in embedded LanceDB, and retrieves audit context in
two passes: semantic nearest neighbors first, then the internal helpers,
modifiers, state variables, and external calls those functions touch.

No API keys. No hosted database. No source code leaves your machine.

## Why auditors use it

Manual review breaks down when a protocol spreads one invariant across
modifiers, libraries, adapters, mocks, and inherited contracts. Plain vector
search helps, but it often returns a relevant function without the dependency
context that makes it dangerous.

`astral` keeps both views:

- **Semantic memory**: "show me withdrawal/reentrancy/oracle code" lands near
  relevant functions even when names differ.
- **Structural memory**: every chunk carries `referenced_symbols`, so retrieval
  expands from the semantic hit to the concrete code it depends on.
- **Topology view**: export the codebase as a 3D graph and hunt for hubs,
  isolated risky functions, low-level calls, and unexpected state coupling.
- **Offline risk analysis**: run vector-space outlier detection and cluster
  deviation analysis over the stored embeddings.

## Install

Requirements:

- macOS or Linux
- Rust stable, installed via [rustup](https://rustup.rs)
- `protoc` from protobuf; on macOS the installer can install it with Homebrew

From a checkout:

```bash
git clone https://github.com/merelinmrelin-web/astral.git
cd astral
scripts/install.sh
```

The installer builds `astral` in release mode and copies the binary to
`~/.local/bin/astral`. Use a different prefix when needed:

```bash
scripts/install.sh --prefix /usr/local
```

If `~/.local/bin` is not on your `PATH`, add:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## Quickstart

```bash
astral ingest path/to/contracts
astral query "reentrancy in withdrawal flow" 5
astral export-graph
open astral_graph.html
```

First ingestion downloads the embedding model into `.fastembed_cache`
(`~90 MB`) and creates `.lancedb_data` in the project directory.

For local development without installing:

```bash
make ingest CONTRACTS=contracts
make query QUERY="low level calls before state update"
make graph
```

## Commands

| Command | Purpose |
| --- | --- |
| `astral parse <dir>` | Parse Solidity files and print semantic chunks as JSON without touching the DB. |
| `astral ingest <dir>` | Parse, embed, and upsert chunks into `.lancedb_data`. |
| `astral query <text> [k]` | Two-pass retrieval as Markdown: primary matches plus dependency context. |
| `astral query-json <text> [k]` | Raw KNN results with distances for retrieval debugging. |
| `astral export-graph [out.json]` | Export topology JSON and a self-contained 3D HTML viewer. |

Add `--include-mocks` before the command to keep `mocks/`, `tests/`, and
`interfaces/` paths in retrieval and graph exports. They are skipped by default.

## Vector risk analysis

`analysis/semantic_risk_analysis.py` runs outlier detection, clustering,
risk-vs-connectivity scoring, and a 2D risk map over the embedding space.

If you have a Parquet dump:

```bash
python3 analysis/semantic_risk_analysis.py \
  --input astral_dump.parquet \
  --out-dir analysis
```

If you want to analyze the local LanceDB table:

```bash
cargo run --bin export_lance_analysis -- \
  .lancedb_data solidity_chunks analysis/lance_dataset_export.json

python3 analysis/semantic_risk_analysis.py \
  --lance-json analysis/lance_dataset_export.json \
  --graph-json astral_graph.json \
  --out-dir analysis
```

Generated reports and maps are intentionally ignored by git.

## Architecture

```text
.sol files
  -> tree-sitter parser
  -> semantic function chunks
  -> fastembed AllMiniLM-L6-v2 embeddings
  -> embedded LanceDB table
  -> semantic KNN + graph expansion
  -> Markdown audit context / 3D topology / offline statistics
```

Core modules:

- `src/parser.rs`: extracts functions, constructors, modifiers,
  `fallback`/`receive`, and referenced symbols.
- `src/chunker.rs`: builds stable chunk IDs and embedding text.
- `src/storage.rs`: owns LanceDB, fastembed, ingestion, KNN, and full-text search.
- `src/retrieval.rs`: assembles primary semantic hits plus dependency context.
- `src/visualizer.rs`: builds the 3D graph and structural risk score.
- `src/bin/export_lance_analysis.rs`: exports vectors for offline Python analysis.

## 3D topology

`astral export-graph` writes:

- `astral_graph.json`: topology data
- `astral_graph.html`: standalone WebGL viewer

Node colors:

| Color | Meaning |
| --- | --- |
| `#ff0055` | body contains a low-level call (`call`, `delegatecall`, `staticcall`, `selfdestruct`) |
| `#4e7cff` | function |
| `#ffd166` | constructor |
| `#8a2be2` | modifier |
| `#ff8c42` | `fallback` / `receive` |
| `#00ffcc` | state variable |
| `#94a3b8` | unresolved external call |

Good first targets: red nodes with many inbound links, isolated red nodes,
state variables touched by unrelated clusters, and forked-standard functions
that sit far from their cluster center in vector analysis.

## Development

```bash
make test
make check
make fmt
```

Useful targets:

| Target | Purpose |
| --- | --- |
| `make install` | Build and install to `$(PREFIX)/bin`, default `~/.local/bin`. |
| `make build` | Release-build the CLI. |
| `make test` | Run installer smoke tests and Rust tests. |
| `make check` | Compile all targets without producing release artifacts. |
| `make graph` | Export the current local topology viewer. |
| `make analyze` | Export LanceDB rows and run vector risk analysis. |
| `make clean-local` | Remove local DB/model/report/viewer artifacts. |

## Troubleshooting

**`protoc` is missing**

Install protobuf:

```bash
brew install protobuf          # macOS
sudo apt-get install protobuf-compiler  # Debian/Ubuntu
```

**First ingest is slow**

The first run downloads and initializes the embedding model. Later runs reuse
`.fastembed_cache`.

**Query results include mocks**

By default, mocks/tests/interfaces are filtered out. If you need them, run:

```bash
astral --include-mocks query "endpoint delegate flow" 10
```

**The graph opens blank from a remote browser**

Open `astral_graph.html` from the same machine that generated it. The viewer is
self-contained and designed for local `file://` use.
