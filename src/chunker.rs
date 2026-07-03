//! Semantic Chunker Module.
//!
//! Turns raw [`FunctionChunk`]s from the parser into embedding-ready
//! [`SemanticChunk`]s: a stable identity for idempotent re-ingestion plus a
//! text representation tuned for the embedding model. The text leads with a
//! natural-language description (contract, function, referenced symbols)
//! before the code, so that queries phrased in prose — "pool initialization
//! in LiquidityPool" — land near the right function in vector space even
//! when they share no tokens with the code itself.

use crate::parser::FunctionChunk;

/// Embedding models used here have a ~512-token window; text beyond it is
/// silently truncated by the tokenizer. We cap the code section ourselves so
/// truncation eats the tail of the code, never the descriptive header.
const MAX_CODE_CHARS_FOR_EMBEDDING: usize = 4000;

/// A chunk ready for ingestion: what gets embedded plus what gets stored.
#[derive(Debug, Clone)]
pub struct SemanticChunk {
    /// Stable identity: `file_path::contract::function::start_line`.
    /// Deterministic so re-ingesting the same codebase upserts instead of
    /// duplicating; the line number disambiguates overloaded functions.
    pub id: String,
    /// Text sent to the embedding model. The full, untruncated code lives in
    /// `source.code_content`.
    pub embedding_text: String,
    pub source: FunctionChunk,
}

pub fn chunk(function: FunctionChunk) -> SemanticChunk {
    SemanticChunk {
        id: format!(
            "{}::{}::{}::{}",
            function.file_path, function.contract_name, function.function_name, function.start_line
        ),
        embedding_text: embedding_text(&function),
        source: function,
    }
}

pub fn chunk_all(functions: impl IntoIterator<Item = FunctionChunk>) -> Vec<SemanticChunk> {
    functions.into_iter().map(chunk).collect()
}

fn embedding_text(function: &FunctionChunk) -> String {
    let mut text = format!(
        "Solidity {kind} `{function}` in contract `{contract}` (file {file}).\n",
        kind = kind_label(function),
        function = function.function_name,
        contract = function.contract_name,
        file = function.file_path,
    );

    if !function.referenced_symbols.is_empty() {
        text.push_str("References: ");
        text.push_str(&function.referenced_symbols.join(", "));
        text.push_str(".\n");
    }

    text.push_str("Code:\n");
    text.push_str(truncate_on_char_boundary(
        &function.code_content,
        MAX_CODE_CHARS_FOR_EMBEDDING,
    ));
    text
}

fn kind_label(function: &FunctionChunk) -> &'static str {
    use crate::parser::DefinitionKind::*;
    match function.kind {
        Function => "function",
        Constructor => "constructor",
        Modifier => "modifier",
        FallbackOrReceive => "fallback/receive function",
    }
}

fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{DefinitionKind, FunctionChunk};

    fn sample_function() -> FunctionChunk {
        FunctionChunk {
            file_path: "contracts/Vault.sol".into(),
            contract_name: "Vault".into(),
            function_name: "withdraw".into(),
            code_content: "function withdraw() external { /* ... */ }".into(),
            referenced_symbols: vec!["balances".into(), "msg.sender.call".into()],
            kind: DefinitionKind::Function,
            start_line: 11,
            end_line: 16,
        }
    }

    #[test]
    fn id_is_stable_and_unique_per_definition() {
        let chunk = chunk(sample_function());
        assert_eq!(chunk.id, "contracts/Vault.sol::Vault::withdraw::11");
    }

    #[test]
    fn embedding_text_leads_with_description() {
        let chunk = chunk(sample_function());
        let text = &chunk.embedding_text;
        assert!(text.starts_with("Solidity function `withdraw` in contract `Vault`"));
        assert!(text.contains("References: balances, msg.sender.call."));
        assert!(text.contains("function withdraw()"));
    }

    #[test]
    fn oversized_code_is_truncated_only_in_embedding_text() {
        let mut function = sample_function();
        function.code_content = "я".repeat(MAX_CODE_CHARS_FOR_EMBEDDING); // 2 bytes each
        let chunk = chunk(function.clone());
        assert!(chunk.embedding_text.len() < function.code_content.len() + 200);
        assert_eq!(chunk.source.code_content, function.code_content);
    }
}
