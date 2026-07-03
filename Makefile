PREFIX ?= $(HOME)/.local
CONTRACTS ?= contracts
QUERY ?= reentrancy in withdrawal flow

.PHONY: install build test check fmt ingest query graph analyze clean-local

install:
	./scripts/install.sh --prefix "$(PREFIX)"

build:
	cargo build --release --bin astral

test:
	bash scripts/test_install.sh
	cargo test

check:
	cargo check --all-targets

fmt:
	cargo fmt --check

ingest:
	cargo run -- ingest "$(CONTRACTS)"

query:
	cargo run -- query "$(QUERY)" 5

graph:
	cargo run -- export-graph

analyze:
	cargo run --bin export_lance_analysis -- .lancedb_data solidity_chunks analysis/lance_dataset_export.json
	python3 analysis/semantic_risk_analysis.py --lance-json analysis/lance_dataset_export.json --graph-json astral_graph.json --out-dir analysis

clean-local:
	rm -rf .lancedb_data .fastembed_cache target/release/astral astral_graph.json astral_graph.html analysis/lance_dataset_export.json analysis/semantic_risk_report.json analysis/semantic_risk_report.md analysis/semantic_risk_map.png
