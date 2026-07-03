mod chunker;
mod parser;
mod retrieval;
mod storage;
mod visualizer;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use parser::SolidityParser;
use retrieval::RetrievalOptions;
use storage::VectorStore;

const DB_DIR: &str = ".lancedb_data";
const TABLE: &str = "solidity_chunks";

const USAGE: &str = "\
usage:
  astral parse        <dir>          print extracted chunks as JSON (no DB)
  astral ingest       <dir>          parse, embed and upsert into .lancedb_data
  astral query        <text> [k]     two-pass retrieval, assembled Markdown context
  astral query-json   <text> [k]     raw KNN results as JSON (retrieval debugging)
  astral export-graph [out.json]     export 3D topology (JSON + three.js viewer)

flags:
  --include-mocks    keep chunks from mocks/, tests/ and interfaces/ paths
                     (both retrieval passes and graph export skip them by default)";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let include_auxiliary = {
        let before = args.len();
        args.retain(|a| a != "--include-mocks");
        args.len() != before
    };
    let options = RetrievalOptions { include_auxiliary };

    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["parse", dir] => parse(Path::new(dir)),
        ["ingest", dir] => ingest(Path::new(dir)).await,
        ["query", text] => query(text, 5, options).await,
        ["query", text, k] => match k.parse() {
            Ok(k) => query(text, k, options).await,
            Err(_) => Err(anyhow::anyhow!("k must be a positive integer, got `{k}`")),
        },
        ["query-json", text] => query_json(text, 5).await,
        ["query-json", text, k] => match k.parse() {
            Ok(k) => query_json(text, k).await,
            Err(_) => Err(anyhow::anyhow!("k must be a positive integer, got `{k}`")),
        },
        ["export-graph"] => export_graph(Path::new("astral_graph.json"), include_auxiliary).await,
        ["export-graph", out] => export_graph(Path::new(out), include_auxiliary).await,
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn parse(dir: &Path) -> anyhow::Result<()> {
    let chunks = SolidityParser::new()?.parse_directory(dir)?;
    println!("{}", serde_json::to_string_pretty(&chunks)?);
    eprintln!("extracted {} chunks from {}", chunks.len(), dir.display());
    Ok(())
}

async fn ingest(dir: &Path) -> anyhow::Result<()> {
    let functions = SolidityParser::new()?.parse_directory(dir)?;
    let chunks = chunker::chunk_all(functions);

    let mut store = VectorStore::open(DB_DIR, TABLE).await?;
    let written = store.ingest(&chunks).await?;
    eprintln!(
        "ingested {written} chunks from {} into {DB_DIR}/{TABLE}",
        dir.display()
    );
    Ok(())
}

async fn query(text: &str, k: usize, options: RetrievalOptions) -> anyhow::Result<()> {
    let store = VectorStore::open(DB_DIR, TABLE).await?;
    let mut assembler = retrieval::ContextAssembler::new(store);
    let context = assembler.assemble(text, k, options).await?;

    println!("{}", context.to_markdown(text));
    eprintln!(
        "{} primary matches, {} dependency chunks (expanded {} symbols)",
        context.primary.len(),
        context.dependencies.len(),
        context.expanded_symbols.len(),
    );
    Ok(())
}

async fn query_json(text: &str, k: usize) -> anyhow::Result<()> {
    let mut store = VectorStore::open(DB_DIR, TABLE).await?;
    let results = store.search(text, k, &[]).await?;
    println!("{}", serde_json::to_string_pretty(&results)?);
    eprintln!("{} results for: {text}", results.len());
    Ok(())
}

async fn export_graph(json_path: &Path, include_auxiliary: bool) -> anyhow::Result<()> {
    let store = VectorStore::open(DB_DIR, TABLE).await?;
    let mut chunks = store.all_chunks().await?;
    if !include_auxiliary {
        chunks.retain(|c| {
            !retrieval::AUXILIARY_PATH_SEGMENTS
                .iter()
                .any(|seg| c.file_path.contains(seg))
        });
    }

    let graph = visualizer::build_graph(&chunks);
    let html_path: PathBuf = visualizer::write_graph_files(&graph, json_path)?;
    eprintln!(
        "exported {} nodes, {} links from {} chunks",
        graph.nodes.len(),
        graph.links.len(),
        chunks.len(),
    );
    eprintln!("  graph:  {}", json_path.display());
    eprintln!("  viewer: {} (open in a browser)", html_path.display());
    Ok(())
}
