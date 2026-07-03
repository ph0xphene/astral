//! Query & Context Retrieval Engine (Graph-Augmented, multi-pass).
//!
//! Pass 1 — vector KNN over the chunk table: the semantic "core" of the
//! answer. Pass 2 — graph expansion: harvest `referenced_symbols` from the
//! core chunks and fetch, by metadata filter alone (no re-embedding), the
//! definitions they point at — internal functions, guarding modifiers,
//! called contracts. The result is one Markdown document where an auditing
//! LLM sees a function *together with* the code it depends on, labeled so
//! primary evidence and supporting context are distinguishable.

use std::collections::{BTreeMap, BTreeSet};

use crate::storage::{FtsResult, SearchResult, StorageError, StoredChunk, VectorStore};

/// Upper bound on chunks fetched by the graph pass, so one hub symbol
/// (a popular interface name, a ubiquitous modifier) cannot flood the
/// context window.
const MAX_DEPENDENCY_CHUNKS: usize = 24;

/// Path segments treated as auxiliary code: test doubles and bodiless
/// interface declarations. Same-named functions from these files
/// (`MockAggregatorV3.latestRoundData` next to the real oracle) are noise
/// for an audit context, so both retrieval passes skip them by default.
pub(crate) const AUXILIARY_PATH_SEGMENTS: &[&str] =
    &["mocks/", "mock/", "tests/", "test/", "interfaces/"];

/// Knobs for [`ContextAssembler::assemble`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RetrievalOptions {
    /// Include chunks from mock/test/interface paths instead of filtering
    /// them out (useful when auditing the test doubles themselves).
    pub include_auxiliary: bool,
}

/// Solidity built-ins and EVM globals that appear as callees but never have
/// a definition chunk in the table — filtered out before the metadata query.
pub(crate) const BUILTINS: &[&str] = &[
    "require",
    "assert",
    "revert",
    "keccak256",
    "sha256",
    "ripemd160",
    "ecrecover",
    "addmod",
    "mulmod",
    "selfdestruct",
    "payable",
    "address",
    "uint256",
    "uint128",
    "uint64",
    "uint32",
    "uint8",
    "int256",
    "bytes32",
    "bytes",
    "string",
    "bool",
    "type",
    "abi",
    "msg",
    "block",
    "tx",
    "super",
    "this",
];

/// RRF rank constant: the standard 60 keeps head ranks dominant without
/// letting a single list swamp the fusion.
const RRF_K: f32 = 60.0;

/// One fused Pass-1 hit: where it ranked in each retrieval channel and the
/// combined Reciprocal Rank Fusion score (higher is better).
#[derive(Debug, Clone)]
pub struct RankedChunk {
    pub chunk: StoredChunk,
    pub rrf_score: f32,
    /// 1-based rank in the vector (semantic) channel, if it appeared there.
    pub vector_rank: Option<usize>,
    /// 1-based rank in the full-text (BM25) channel, if it appeared there.
    pub fts_rank: Option<usize>,
}

/// The assembled two-pass retrieval result.
#[derive(Debug)]
pub struct AssembledContext {
    /// Pass-1 hybrid matches (vector KNN ⊕ BM25 via RRF), best first.
    pub primary: Vec<RankedChunk>,
    /// Pass-2 chunks reached via `referenced_symbols`, deduplicated against
    /// the primary set.
    pub dependencies: Vec<StoredChunk>,
    /// The symbol names the graph pass searched for (useful for debugging
    /// retrieval quality).
    pub expanded_symbols: Vec<String>,
}

impl AssembledContext {
    /// Renders the context as one Markdown document ready to hand to an LLM.
    pub fn to_markdown(&self, query: &str) -> String {
        let mut md = format!("# Retrieved context\n\nQuery: **{query}**\n");

        for result in &self.primary {
            let mut ranks = Vec::new();
            if let Some(r) = result.vector_rank {
                ranks.push(format!("semantic #{r}"));
            }
            if let Some(r) = result.fts_rank {
                ranks.push(format!("text #{r}"));
            }
            let detail = format!(
                "- Retrieval: {} (RRF {:.4})\n",
                ranks.join(", "),
                result.rrf_score,
            );
            md.push_str(&render_chunk(&result.chunk, "PRIMARY MATCH", &detail));
        }

        if !self.dependencies.is_empty() {
            md.push_str(
                "\n---\n\nThe following definitions are referenced by the primary \
                 matches and are included as supporting context.\n",
            );
            for chunk in &self.dependencies {
                md.push_str(&render_chunk(chunk, "DEPENDENCY CONTEXT", ""));
            }
        }

        md
    }
}

/// Two-pass retriever over a [`VectorStore`].
pub struct ContextAssembler {
    store: VectorStore,
}

impl ContextAssembler {
    pub fn new(store: VectorStore) -> Self {
        Self { store }
    }

    /// Runs both passes for `query`: `k` vector matches, then a metadata
    /// fetch of everything their `referenced_symbols` point at.
    pub async fn assemble(
        &mut self,
        query: &str,
        k: usize,
        options: RetrievalOptions,
    ) -> Result<AssembledContext, StorageError> {
        let excluded: &[&str] = if options.include_auxiliary {
            &[]
        } else {
            AUXILIARY_PATH_SEGMENTS
        };

        // Hybrid Pass 1: semantic KNN catches paraphrases, BM25 catches
        // exact identifiers the embedding model dilutes. Each channel
        // over-fetches so the fused top-k has real candidates from both.
        let vector = self.store.search(query, k * 2, excluded).await?;
        let fts = self.store.fts_search(query, k * 2, excluded).await?;
        let primary = fuse_rrf(vector, fts, k);

        let symbols = harvest_symbols(&primary);
        let mut dependencies = self
            .store
            .find_by_names(&symbols, MAX_DEPENDENCY_CHUNKS, excluded)
            .await?;

        // The graph pass may re-find a primary chunk (a top-k function that
        // is itself called by another top-k function) — keep it primary only.
        let primary_ids: BTreeSet<&str> = primary.iter().map(|r| r.chunk.id.as_str()).collect();
        dependencies.retain(|c| !primary_ids.contains(c.id.as_str()));

        sort_by_locality(&mut dependencies, &primary);

        Ok(AssembledContext {
            primary,
            dependencies,
            expanded_symbols: symbols,
        })
    }
}

/// Reciprocal Rank Fusion: each chunk scores `Σ 1/(60 + rank)` across the
/// channels it appears in, so a hit ranked well in both channels beats a
/// hit ranked first in only one. Returns the fused top-`k`.
fn fuse_rrf(vector: Vec<SearchResult>, fts: Vec<FtsResult>, k: usize) -> Vec<RankedChunk> {
    let mut fused: BTreeMap<String, RankedChunk> = BTreeMap::new();

    for (i, result) in vector.into_iter().enumerate() {
        let rank = i + 1;
        fused
            .entry(result.chunk.id.clone())
            .or_insert_with(|| RankedChunk {
                chunk: result.chunk,
                rrf_score: 0.0,
                vector_rank: None,
                fts_rank: None,
            })
            .apply_rank(rank, Channel::Vector);
    }
    for (i, result) in fts.into_iter().enumerate() {
        let rank = i + 1;
        fused
            .entry(result.chunk.id.clone())
            .or_insert_with(|| RankedChunk {
                chunk: result.chunk,
                rrf_score: 0.0,
                vector_rank: None,
                fts_rank: None,
            })
            .apply_rank(rank, Channel::Fts);
    }

    let mut ranked: Vec<RankedChunk> = fused.into_values().collect();
    ranked.sort_by(|a, b| b.rrf_score.total_cmp(&a.rrf_score));
    ranked.truncate(k);
    ranked
}

enum Channel {
    Vector,
    Fts,
}

impl RankedChunk {
    fn apply_rank(&mut self, rank: usize, channel: Channel) {
        self.rrf_score += 1.0 / (RRF_K + rank as f32);
        match channel {
            Channel::Vector => self.vector_rank = Some(rank),
            Channel::Fts => self.fts_rank = Some(rank),
        }
    }
}

/// Orders dependency chunks by how close they are to the primary matches:
/// same contract first, then same file, then everything else. Within a tier
/// the order is deterministic (contract, then function name).
fn sort_by_locality(dependencies: &mut [StoredChunk], primary: &[RankedChunk]) {
    let primary_contracts: BTreeSet<&str> = primary
        .iter()
        .map(|r| r.chunk.contract_name.as_str())
        .collect();
    let primary_files: BTreeSet<&str> =
        primary.iter().map(|r| r.chunk.file_path.as_str()).collect();

    dependencies.sort_by(|a, b| {
        let tier = |c: &StoredChunk| {
            if primary_contracts.contains(c.contract_name.as_str()) {
                0
            } else if primary_files.contains(c.file_path.as_str()) {
                1
            } else {
                2
            }
        };
        tier(a)
            .cmp(&tier(b))
            .then_with(|| a.contract_name.cmp(&b.contract_name))
            .then_with(|| a.function_name.cmp(&b.function_name))
    });
}

/// Collects the candidate definition names from the primary chunks'
/// `referenced_symbols`.
///
/// Symbols arrive in source form: `_checkDeviation`, `onlyOwner`,
/// `token.transfer`, `IERC20(pool)`, `msg.sender.call{value: x}`. For each
/// entry we take the leading identifier (a contract/state-variable name or a
/// plain function/modifier name) and the identifier after the last dot (a
/// method name on an external contract); built-ins and non-identifiers are
/// dropped. Sanitization proper happens again in `find_by_names`.
fn harvest_symbols(primary: &[RankedChunk]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for result in primary {
        for symbol in &result.chunk.referenced_symbols {
            // Cut call/value suffixes: `call{value: x}` → `call`, `f(a)` → `f`.
            let head = symbol.split(['{', '(']).next().unwrap_or(symbol).trim();
            for candidate in [
                head.split('.').next().unwrap_or(head),
                head.rsplit('.').next().unwrap_or(head),
            ] {
                if !candidate.is_empty() && !BUILTINS.contains(&candidate) {
                    names.insert(candidate.to_owned());
                }
            }
        }
    }
    names.into_iter().collect()
}

fn render_chunk(chunk: &StoredChunk, label: &str, detail: &str) -> String {
    let mut md = format!(
        "\n## [{label}] `{contract}.{function}`\n\n",
        contract = chunk.contract_name,
        function = chunk.function_name,
    );

    md.push_str(&format!(
        "- Source: `{}:{}-{}`\n- Kind: {}\n",
        chunk.file_path, chunk.start_line, chunk.end_line, chunk.kind,
    ));
    md.push_str(detail);
    if !chunk.referenced_symbols.is_empty() {
        md.push_str(&format!(
            "- References: {}\n",
            chunk.referenced_symbols.join(", ")
        ));
    }

    md.push_str("\n```solidity\n");
    md.push_str(&chunk.code_content);
    md.push_str("\n```\n");
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_chunk(id: &str, symbols: &[&str]) -> StoredChunk {
        StoredChunk {
            id: id.into(),
            file_path: "f.sol".into(),
            contract_name: "C".into(),
            function_name: "f".into(),
            code_content: "function f() {}".into(),
            referenced_symbols: symbols.iter().map(|s| s.to_string()).collect(),
            kind: "function".into(),
            start_line: 1,
            end_line: 1,
        }
    }

    fn chunk_with_symbols(symbols: &[&str]) -> RankedChunk {
        RankedChunk {
            chunk: stored_chunk("f.sol::C::f::1", symbols),
            rrf_score: 0.5,
            vector_rank: Some(1),
            fts_rank: None,
        }
    }

    #[test]
    fn rrf_ranks_dual_channel_hits_above_single_channel_leaders() {
        let vector = vec![
            SearchResult {
                distance: 0.1,
                chunk: stored_chunk("a", &[]),
            },
            SearchResult {
                distance: 0.2,
                chunk: stored_chunk("b", &[]),
            },
        ];
        let fts = vec![
            FtsResult {
                score: 9.0,
                chunk: stored_chunk("c", &[]),
            },
            FtsResult {
                score: 5.0,
                chunk: stored_chunk("b", &[]),
            },
        ];

        let fused = fuse_rrf(vector, fts, 3);
        // `b` is #2 in both channels: 1/62 + 1/62 > 1/61 of either leader.
        assert_eq!(fused[0].chunk.id, "b");
        assert_eq!(fused[0].vector_rank, Some(2));
        assert_eq!(fused[0].fts_rank, Some(2));
        assert_eq!(fused.len(), 3);
        // Single-channel hits keep their channel rank and lose the other.
        let a = fused.iter().find(|r| r.chunk.id == "a").expect("a fused");
        assert_eq!((a.vector_rank, a.fts_rank), (Some(1), None));
    }

    #[test]
    fn harvests_plain_member_and_lowlevel_symbols() {
        let primary = vec![chunk_with_symbols(&[
            "_checkDeviation",
            "onlyOwner",
            "token.transfer",
            "msg.sender.call{value: amount}",
            "require",
        ])];
        let symbols = harvest_symbols(&primary);
        assert!(symbols.contains(&"_checkDeviation".to_owned()));
        assert!(symbols.contains(&"onlyOwner".to_owned()));
        assert!(symbols.contains(&"token".to_owned()));
        assert!(symbols.contains(&"transfer".to_owned()));
        // Built-ins and globals must not leak into the metadata query.
        assert!(!symbols.contains(&"require".to_owned()));
        assert!(!symbols.contains(&"msg".to_owned()));
    }

    #[test]
    fn dependencies_sorted_same_contract_then_same_file_then_rest() {
        let mut primary = chunk_with_symbols(&[]);
        primary.chunk.contract_name = "Oracle".into();
        primary.chunk.file_path = "contracts/Oracle.sol".into();

        let dep = |contract: &str, file: &str, function: &str| StoredChunk {
            contract_name: contract.into(),
            file_path: file.into(),
            function_name: function.into(),
            ..chunk_with_symbols(&[]).chunk
        };
        let mut deps = vec![
            dep("MockFeed", "mocks/MockFeed.sol", "latestRoundData"),
            dep("OracleLib", "contracts/Oracle.sol", "scale"),
            dep("Oracle", "contracts/Oracle.sol", "_checkDeviation"),
        ];

        sort_by_locality(&mut deps, &[primary]);
        let order: Vec<&str> = deps.iter().map(|c| c.function_name.as_str()).collect();
        assert_eq!(order, vec!["_checkDeviation", "scale", "latestRoundData"]);
    }

    #[test]
    fn markdown_labels_primary_and_dependency_sections() {
        let context = AssembledContext {
            primary: vec![chunk_with_symbols(&["_helper"])],
            dependencies: vec![StoredChunk {
                function_name: "_helper".into(),
                ..chunk_with_symbols(&[]).chunk
            }],
            expanded_symbols: vec!["_helper".into()],
        };
        let md = context.to_markdown("test query");
        let primary_pos = md.find("[PRIMARY MATCH] `C.f`").expect("primary section");
        let dep_pos = md
            .find("[DEPENDENCY CONTEXT] `C._helper`")
            .expect("dependency section");
        assert!(primary_pos < dep_pos);
        assert!(md.contains("```solidity"));
    }
}
