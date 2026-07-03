//! Vector Ingestion & Storage Module (LanceDB).
//!
//! Owns the embedded LanceDB table in `.lancedb_data` and the local
//! `fastembed` model. Ingestion is an upsert (merge-insert on the chunk
//! `id`), so re-indexing a codebase after edits updates rows in place
//! instead of accumulating duplicates. Search embeds the query with the
//! same model and runs a KNN scan, returning full chunk metadata so the
//! retrieval engine can follow `referenced_symbols` to related entities.

use std::sync::Arc;

use arrow_array::builder::{ListBuilder, StringBuilder};
use arrow_array::types::Float32Type;
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, ListArray, RecordBatch, RecordBatchIterator,
    StringArray, UInt32Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use futures::TryStreamExt;
use lancedb::Table;
use lancedb::index::Index;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::query::{ExecutableQuery, QueryBase};
use thiserror::Error;

use crate::chunker::SemanticChunk;

/// 384-dimensional, fast on CPU and good enough for code+prose retrieval;
/// swap for a larger model here once quality becomes the bottleneck. The
/// ONNX weights (~90 MB) are downloaded on first use and cached locally.
const EMBEDDING_MODEL: EmbeddingModel = EmbeddingModel::AllMiniLML6V2;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("LanceDB operation failed")]
    Lance(#[from] lancedb::Error),

    #[error("embedding model failed")]
    Embedding(#[from] fastembed::Error),

    #[error("failed to build Arrow record batch")]
    Arrow(#[from] arrow_schema::ArrowError),

    #[error("stored row is malformed: bad or missing column `{0}`")]
    MalformedColumn(&'static str),
}

/// Chunk metadata as read back from the table — the retrieval-side view of
/// [`SemanticChunk`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredChunk {
    pub id: String,
    pub file_path: String,
    pub contract_name: String,
    pub function_name: String,
    pub code_content: String,
    pub referenced_symbols: Vec<String>,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    /// L2 distance from the query vector; smaller is more similar.
    pub distance: f32,
    pub chunk: StoredChunk,
}

/// A full-text (BM25) hit; `score` is higher-is-better, unlike `distance`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FtsResult {
    pub score: f32,
    pub chunk: StoredChunk,
}

/// Embedded vector store: LanceDB table + local embedding model.
///
/// Methods take `&mut self` because `fastembed` mutates internal ONNX
/// session state; wrap in a `Mutex` if you need shared access.
pub struct VectorStore {
    table: Table,
    embedder: TextEmbedding,
    schema: SchemaRef,
}

impl VectorStore {
    /// Opens (or creates) the table `table_name` in the LanceDB directory
    /// `db_dir`, e.g. `.lancedb_data`. Loads the embedding model, downloading
    /// it on first run.
    pub async fn open(db_dir: &str, table_name: &str) -> Result<Self, StorageError> {
        let dim = TextEmbedding::get_model_info(&EMBEDDING_MODEL)?.dim;
        let embedder = TextEmbedding::try_new(
            TextInitOptions::new(EMBEDDING_MODEL).with_show_download_progress(true),
        )?;
        let schema = chunk_schema(dim);

        let db = lancedb::connect(db_dir).execute().await?;
        let existing = db.table_names().execute().await?;
        let table = if existing.iter().any(|n| n == table_name) {
            db.open_table(table_name).execute().await?
        } else {
            db.create_empty_table(table_name, schema.clone())
                .execute()
                .await?
        };

        Ok(Self {
            table,
            embedder,
            schema,
        })
    }

    /// Embeds and upserts the given chunks. Returns the number written.
    pub async fn ingest(&mut self, chunks: &[SemanticChunk]) -> Result<usize, StorageError> {
        if chunks.is_empty() {
            return Ok(0);
        }

        let texts: Vec<&str> = chunks.iter().map(|c| c.embedding_text.as_str()).collect();
        let vectors = self.embedder.embed(&texts, None)?;

        let batch = build_record_batch(self.schema.clone(), chunks, &vectors)?;
        let reader = RecordBatchIterator::new([Ok(batch)], self.schema.clone());

        // Merge on `id`: re-ingesting a re-parsed codebase overwrites stale
        // rows and inserts new ones in a single atomic operation.
        let mut merge = self.table.merge_insert(&["id"]);
        merge
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge.execute(Box::new(reader)).await?;

        // (Re)build the BM25 inverted index over the code so hybrid search
        // can do exact-token matching (`safeTransferFrom`, `nonReentrant`).
        // The default tokenizer splits on punctuation/whitespace only, so
        // camelCase identifiers survive as single searchable tokens.
        self.table
            .create_index(&["code_content"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await?;

        Ok(chunks.len())
    }

    /// Metadata-only lookup for the graph pass — no embedding involved.
    /// Returns chunks whose `function_name` or `contract_name` equals any of
    /// `names`. Names that are not plain Solidity identifiers are dropped
    /// (they cannot match a name column and would break the SQL filter).
    /// Chunks whose `file_path` contains any of `exclude_path_segments` are
    /// filtered out on the database side.
    pub async fn find_by_names(
        &self,
        names: &[String],
        limit: usize,
        exclude_path_segments: &[&str],
    ) -> Result<Vec<StoredChunk>, StorageError> {
        let quoted: Vec<String> = names
            .iter()
            .filter(|n| is_solidity_identifier(n))
            .map(|n| format!("'{n}'"))
            .collect();
        if quoted.is_empty() {
            return Ok(Vec::new());
        }

        let list = quoted.join(", ");
        let mut filter = format!("(function_name IN ({list}) OR contract_name IN ({list}))");
        if let Some(exclusion) = path_exclusion_filter(exclude_path_segments) {
            filter = format!("{filter} AND {exclusion}");
        }

        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .only_if(filter)
            .limit(limit)
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut chunks = Vec::new();
        for batch in &batches {
            chunks.extend(parse_chunk_rows(batch)?);
        }
        Ok(chunks)
    }

    /// Full table scan: every stored chunk, for offline exports (graph
    /// visualization, reports). Not for the retrieval path.
    pub async fn all_chunks(&self) -> Result<Vec<StoredChunk>, StorageError> {
        let total = self.table.count_rows(None).await?;
        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .limit(total.max(1))
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut chunks = Vec::new();
        for batch in &batches {
            chunks.extend(parse_chunk_rows(batch)?);
        }
        Ok(chunks)
    }

    /// Full-text (BM25) search over `code_content`, best score first.
    /// Requires the FTS index that `ingest` builds; a table created before
    /// hybrid search existed must be re-ingested once.
    pub async fn fts_search(
        &self,
        query: &str,
        k: usize,
        exclude_path_segments: &[&str],
    ) -> Result<Vec<FtsResult>, StorageError> {
        let mut search = self
            .table
            .query()
            .full_text_search(FullTextSearchQuery::new(query.to_owned()))
            .limit(k);
        if let Some(exclusion) = path_exclusion_filter(exclude_path_segments) {
            search = search.only_if(exclusion);
        }

        let batches: Vec<RecordBatch> = search.execute().await?.try_collect().await?;

        let mut results = Vec::new();
        for batch in &batches {
            let scores: &Float32Array = batch
                .column_by_name("_score")
                .and_then(|c| c.as_any().downcast_ref())
                .ok_or(StorageError::MalformedColumn("_score"))?;
            results.extend(
                parse_chunk_rows(batch)?
                    .into_iter()
                    .enumerate()
                    .map(|(row, chunk)| FtsResult {
                        score: scores.value(row),
                        chunk,
                    }),
            );
        }
        results.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(results)
    }

    /// KNN search: embeds `query` and returns the `k` nearest chunks with
    /// full metadata, closest first. Chunks under `exclude_path_segments`
    /// are filtered before the KNN limit is applied, so `k` real matches
    /// come back even when mocks would have dominated the neighborhood.
    pub async fn search(
        &mut self,
        query: &str,
        k: usize,
        exclude_path_segments: &[&str],
    ) -> Result<Vec<SearchResult>, StorageError> {
        let query_vector = self
            .embedder
            .embed(&[query], None)?
            .into_iter()
            .next()
            .expect("fastembed returns one embedding per input text");

        let mut search = self.table.vector_search(query_vector)?.limit(k);
        if let Some(exclusion) = path_exclusion_filter(exclude_path_segments) {
            search = search.only_if(exclusion);
        }

        let batches: Vec<RecordBatch> = search.execute().await?.try_collect().await?;

        let mut results = Vec::new();
        for batch in &batches {
            results.extend(parse_search_batch(batch)?);
        }
        results.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        Ok(results)
    }
}

fn chunk_schema(dim: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            false,
        ),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("contract_name", DataType::Utf8, false),
        Field::new("function_name", DataType::Utf8, false),
        Field::new("code_content", DataType::Utf8, false),
        Field::new(
            "referenced_symbols",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            false,
        ),
        Field::new("kind", DataType::Utf8, false),
        Field::new("start_line", DataType::UInt32, false),
        Field::new("end_line", DataType::UInt32, false),
    ]))
}

fn build_record_batch(
    schema: SchemaRef,
    chunks: &[SemanticChunk],
    vectors: &[Vec<f32>],
) -> Result<RecordBatch, StorageError> {
    let dim = vectors.first().map_or(0, Vec::len) as i32;
    let vector_col = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vectors
            .iter()
            .map(|v| Some(v.iter().copied().map(Some).collect::<Vec<_>>())),
        dim,
    );

    let mut symbols = ListBuilder::new(StringBuilder::new());
    for chunk in chunks {
        for symbol in &chunk.source.referenced_symbols {
            symbols.values().append_value(symbol);
        }
        symbols.append(true);
    }

    let str_col = |f: fn(&SemanticChunk) -> &str| {
        Arc::new(StringArray::from_iter_values(chunks.iter().map(f))) as Arc<dyn Array>
    };

    let batch = RecordBatch::try_new(
        schema,
        vec![
            str_col(|c| &c.id),
            Arc::new(vector_col),
            str_col(|c| &c.source.file_path),
            str_col(|c| &c.source.contract_name),
            str_col(|c| &c.source.function_name),
            str_col(|c| &c.source.code_content),
            Arc::new(symbols.finish()),
            str_col(|c| c.source.kind.as_str()),
            Arc::new(UInt32Array::from_iter_values(
                chunks.iter().map(|c| c.source.start_line as u32),
            )),
            Arc::new(UInt32Array::from_iter_values(
                chunks.iter().map(|c| c.source.end_line as u32),
            )),
        ],
    )?;
    Ok(batch)
}

/// Builds a SQL predicate excluding rows whose `file_path` contains any of
/// the given segments, or `None` when there is nothing to exclude. Segments
/// are restricted to path-safe characters, so they cannot break the literal.
fn path_exclusion_filter(segments: &[&str]) -> Option<String> {
    let predicates: Vec<String> = segments
        .iter()
        .filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
        })
        .map(|s| format!("file_path NOT LIKE '%{s}%'"))
        .collect();
    (!predicates.is_empty()).then(|| format!("({})", predicates.join(" AND ")))
}

/// `true` for names that could be a Solidity identifier (and are therefore
/// safe to embed in a LanceDB SQL filter literal).
fn is_solidity_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Decodes every row of a batch back into [`StoredChunk`]s.
fn parse_chunk_rows(batch: &RecordBatch) -> Result<Vec<StoredChunk>, StorageError> {
    let string = |name: &'static str| -> Result<&StringArray, StorageError> {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref())
            .ok_or(StorageError::MalformedColumn(name))
    };
    let uint = |name: &'static str| -> Result<&UInt32Array, StorageError> {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref())
            .ok_or(StorageError::MalformedColumn(name))
    };

    let ids = string("id")?;
    let file_paths = string("file_path")?;
    let contracts = string("contract_name")?;
    let functions = string("function_name")?;
    let code = string("code_content")?;
    let kinds = string("kind")?;
    let start_lines = uint("start_line")?;
    let end_lines = uint("end_line")?;
    let symbols: &ListArray = batch
        .column_by_name("referenced_symbols")
        .and_then(|c| c.as_any().downcast_ref())
        .ok_or(StorageError::MalformedColumn("referenced_symbols"))?;

    (0..batch.num_rows())
        .map(|row| {
            let row_symbols = symbols.value(row);
            let row_symbols: &StringArray = row_symbols
                .as_any()
                .downcast_ref()
                .ok_or(StorageError::MalformedColumn("referenced_symbols"))?;

            Ok(StoredChunk {
                id: ids.value(row).to_owned(),
                file_path: file_paths.value(row).to_owned(),
                contract_name: contracts.value(row).to_owned(),
                function_name: functions.value(row).to_owned(),
                code_content: code.value(row).to_owned(),
                referenced_symbols: row_symbols.iter().flatten().map(str::to_owned).collect(),
                kind: kinds.value(row).to_owned(),
                start_line: start_lines.value(row),
                end_line: end_lines.value(row),
            })
        })
        .collect()
}

fn parse_search_batch(batch: &RecordBatch) -> Result<Vec<SearchResult>, StorageError> {
    let distances: &Float32Array = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref())
        .ok_or(StorageError::MalformedColumn("_distance"))?;

    Ok(parse_chunk_rows(batch)?
        .into_iter()
        .enumerate()
        .map(|(row, chunk)| SearchResult {
            distance: distances.value(row),
            chunk,
        })
        .collect())
}
