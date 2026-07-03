//! AST Parser Module.
//!
//! Reads Solidity sources and extracts one semantic chunk per function-like
//! definition (functions, constructors, modifiers, fallback/receive) using
//! `tree-sitter-solidity`. Each chunk carries the contract it belongs to,
//! the raw code of the definition and the symbols it references — external
//! calls and state variables it reads or writes — so the downstream chunker
//! and retrieval engine can reassemble cross-contract context.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;
use tree_sitter::{Node, Parser};
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum ParserError {
    #[error("failed to read source file `{path}`")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to load the Solidity grammar")]
    Language(#[from] tree_sitter::LanguageError),

    #[error("tree-sitter returned no syntax tree for `{0}` (parser misconfigured or cancelled)")]
    ParseFailed(PathBuf),
}

/// What kind of function-like definition a chunk was extracted from.
///
/// Constructors, modifiers and fallback/receive are first-class here because
/// a large share of real-world vulnerabilities (unprotected initializers,
/// reentrancy through `receive`, broken auth modifiers) live exactly in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionKind {
    Function,
    Constructor,
    Modifier,
    FallbackOrReceive,
}

impl DefinitionKind {
    /// Stable string form used in the LanceDB `kind` column; matches the
    /// serde `snake_case` representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Constructor => "constructor",
            Self::Modifier => "modifier",
            Self::FallbackOrReceive => "fallback_or_receive",
        }
    }
}

/// A semantic chunk: one function-like definition with its context.
///
/// Field names deliberately match the metadata schema stored in LanceDB, so
/// this struct serializes directly into the table payload.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionChunk {
    pub file_path: String,
    pub contract_name: String,
    pub function_name: String,
    pub code_content: String,
    /// Callees of every call expression in the body plus every state variable
    /// of the enclosing contract that the body reads or writes. Sorted and
    /// deduplicated.
    pub referenced_symbols: Vec<String>,
    pub kind: DefinitionKind,
    /// 1-based line range in the source file, for showing provenance in
    /// retrieval results.
    pub start_line: usize,
    pub end_line: usize,
}

/// Wraps a `tree_sitter::Parser` configured with the Solidity grammar.
///
/// `tree_sitter::Parser` is stateful, hence the `&mut self` on parse methods;
/// create one `SolidityParser` per thread if you parallelize ingestion.
pub struct SolidityParser {
    parser: Parser,
}

impl SolidityParser {
    pub fn new() -> Result<Self, ParserError> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_solidity::LANGUAGE.into())?;
        Ok(Self { parser })
    }

    /// Recursively parses every `.sol` file under `dir`.
    ///
    /// I/O and parse failures of individual files are returned as errors
    /// immediately; unreadable directory entries are skipped.
    pub fn parse_directory(&mut self, dir: &Path) -> Result<Vec<FunctionChunk>, ParserError> {
        let mut chunks = Vec::new();
        for entry in WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "sol"))
        {
            chunks.extend(self.parse_file(entry.path())?);
        }
        Ok(chunks)
    }

    pub fn parse_file(&mut self, path: &Path) -> Result<Vec<FunctionChunk>, ParserError> {
        let source = std::fs::read_to_string(path).map_err(|source| ParserError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        self.parse_source(&source, &path.to_string_lossy())
    }

    /// Parses in-memory Solidity source. `file_path` is only recorded in the
    /// resulting chunks, the file is not touched.
    pub fn parse_source(
        &mut self,
        source: &str,
        file_path: &str,
    ) -> Result<Vec<FunctionChunk>, ParserError> {
        let tree = self
            .parser
            .parse(source, None)
            .ok_or_else(|| ParserError::ParseFailed(PathBuf::from(file_path)))?;

        let mut chunks = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if let Some(contract) = ContractNode::match_node(child) {
                contract.extract_chunks(source, file_path, &mut chunks);
            }
        }
        Ok(chunks)
    }
}

/// A `contract`/`interface`/`library` declaration found at the top level.
struct ContractNode<'tree> {
    node: Node<'tree>,
}

impl<'tree> ContractNode<'tree> {
    fn match_node(node: Node<'tree>) -> Option<Self> {
        matches!(
            node.kind(),
            "contract_declaration" | "interface_declaration" | "library_declaration"
        )
        .then_some(Self { node })
    }

    fn extract_chunks(&self, source: &str, file_path: &str, out: &mut Vec<FunctionChunk>) {
        let contract_name = self
            .node
            .child_by_field_name("name")
            .map(|n| node_text(n, source).to_owned())
            .unwrap_or_else(|| "<anonymous>".to_owned());

        let Some(body) = self.node.child_by_field_name("body") else {
            return; // e.g. `interface IERC20;` without a body
        };

        // State variable names are collected first so that each function's
        // symbol pass can tell state reads/writes apart from locals.
        let state_vars = collect_state_variable_names(body, source);

        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            let kind = match member.kind() {
                "function_definition" => DefinitionKind::Function,
                "constructor_definition" => DefinitionKind::Constructor,
                "modifier_definition" => DefinitionKind::Modifier,
                "fallback_receive_definition" => DefinitionKind::FallbackOrReceive,
                _ => continue,
            };

            let function_name = match kind {
                DefinitionKind::Constructor => "constructor".to_owned(),
                DefinitionKind::FallbackOrReceive => fallback_or_receive_name(member, source),
                _ => member
                    .child_by_field_name("name")
                    .map(|n| node_text(n, source).to_owned())
                    .unwrap_or_else(|| "<anonymous>".to_owned()),
            };

            out.push(FunctionChunk {
                file_path: file_path.to_owned(),
                contract_name: contract_name.clone(),
                function_name,
                code_content: node_text(member, source).to_owned(),
                referenced_symbols: collect_referenced_symbols(member, source, &state_vars),
                kind,
                start_line: member.start_position().row + 1,
                end_line: member.end_position().row + 1,
            });
        }
    }
}

fn node_text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    &source[node.byte_range()]
}

/// Names of all state variables declared directly in the contract body.
fn collect_state_variable_names(body: Node<'_>, source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut cursor = body.walk();
    for member in body.children(&mut cursor) {
        if member.kind() == "state_variable_declaration" {
            if let Some(name) = member.child_by_field_name("name") {
                names.insert(node_text(name, source).to_owned());
            }
        }
    }
    names
}

/// Walks a function-like definition and gathers the symbols it references:
/// the callee of every call expression (`token.transfer`, `_burn`, ...) and
/// every identifier that names a state variable of the enclosing contract.
fn collect_referenced_symbols(
    definition: Node<'_>,
    source: &str,
    state_vars: &BTreeSet<String>,
) -> Vec<String> {
    let mut symbols = BTreeSet::new();
    // Manual stack instead of recursion: deeply nested expressions in
    // generated/obfuscated contracts would otherwise overflow the call stack.
    let mut stack = vec![definition];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function") {
                    symbols.insert(node_text(callee, source).to_owned());
                }
            }
            // Modifiers guarding the function (`onlyOwner`, `nonReentrant`)
            // are part of its security surface; the graph pass uses these to
            // pull the modifier's own code into the audit context.
            "modifier_invocation" => {
                let mut cursor = node.walk();
                if let Some(name) = node
                    .children(&mut cursor)
                    .find(|c| c.kind() == "identifier")
                {
                    symbols.insert(node_text(name, source).to_owned());
                }
            }
            "identifier" => {
                let text = node_text(node, source);
                if state_vars.contains(text) {
                    symbols.insert(text.to_owned());
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    symbols.into_iter().collect()
}

fn fallback_or_receive_name(node: Node<'_>, source: &str) -> String {
    // The grammar folds `fallback()` and `receive()` into one node kind;
    // recover which one it is from the keyword token.
    let text = node_text(node, source);
    if text.trim_start().starts_with("receive") {
        "receive".to_owned()
    } else {
        "fallback".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
}

contract LiquidityPool {
    IERC20 public token;
    uint256 public totalShares;
    mapping(address => uint256) public shares;
    bool private initialized;

    modifier onlyInitialized() {
        require(initialized, "not initialized");
        _;
    }

    constructor(address _token) {
        token = IERC20(_token);
    }

    function initialize(uint256 initialShares) external {
        initialized = true;
        totalShares = initialShares;
        shares[msg.sender] = initialShares;
    }

    function withdraw(uint256 amount) external onlyInitialized {
        shares[msg.sender] -= amount;
        totalShares -= amount;
        token.transfer(msg.sender, amount);
    }

    receive() external payable {}
}
"#;

    fn parse_sample() -> Vec<FunctionChunk> {
        SolidityParser::new()
            .expect("grammar must load")
            .parse_source(SAMPLE, "LiquidityPool.sol")
            .expect("sample must parse")
    }

    #[test]
    fn extracts_all_function_like_definitions() {
        let chunks = parse_sample();
        let names: Vec<(&str, &str)> = chunks
            .iter()
            .map(|c| (c.contract_name.as_str(), c.function_name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("IERC20", "transfer"),
                ("LiquidityPool", "onlyInitialized"),
                ("LiquidityPool", "constructor"),
                ("LiquidityPool", "initialize"),
                ("LiquidityPool", "withdraw"),
                ("LiquidityPool", "receive"),
            ]
        );
    }

    #[test]
    fn tracks_state_reads_writes_and_external_calls() {
        let chunks = parse_sample();
        let withdraw = chunks
            .iter()
            .find(|c| c.function_name == "withdraw")
            .expect("withdraw chunk");
        assert_eq!(
            withdraw.referenced_symbols,
            vec![
                "onlyInitialized",
                "shares",
                "token",
                "token.transfer",
                "totalShares"
            ]
        );
        assert_eq!(withdraw.kind, DefinitionKind::Function);
        assert!(withdraw.code_content.starts_with("function withdraw"));
    }

    #[test]
    fn records_line_provenance() {
        let chunks = parse_sample();
        let ctor = chunks
            .iter()
            .find(|c| c.function_name == "constructor")
            .expect("constructor chunk");
        assert!(ctor.start_line > 1 && ctor.end_line >= ctor.start_line);
    }
}
