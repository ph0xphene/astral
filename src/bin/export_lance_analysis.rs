use arrow_array::{
    Array, FixedSizeListArray, Float32Array, ListArray, RecordBatch, StringArray, UInt32Array,
};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
struct AnalysisRow {
    id: String,
    vector: Vec<f32>,
    file_path: String,
    contract_name: String,
    function_name: String,
    code_content: String,
    referenced_symbols: Vec<String>,
    kind: String,
    start_line: u32,
    end_line: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let db_dir = args.next().unwrap_or_else(|| ".lancedb_data".to_owned());
    let table_name = args.next().unwrap_or_else(|| "solidity_chunks".to_owned());
    let out_path = args
        .next()
        .unwrap_or_else(|| "analysis/lance_dataset_export.json".to_owned());

    let db = lancedb::connect(&db_dir).execute().await?;
    let table = db.open_table(&table_name).execute().await?;
    let total = table.count_rows(None).await?;
    let batches: Vec<RecordBatch> = table
        .query()
        .limit(total.max(1))
        .execute()
        .await?
        .try_collect()
        .await?;

    let mut rows = Vec::new();
    for batch in &batches {
        rows.extend(parse_batch(batch)?);
    }

    if let Some(parent) = Path::new(&out_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out_path, serde_json::to_string_pretty(&rows)?)?;
    eprintln!("exported {} rows to {}", rows.len(), out_path);
    Ok(())
}

fn parse_batch(batch: &RecordBatch) -> anyhow::Result<Vec<AnalysisRow>> {
    let strings = |name: &str| -> anyhow::Result<&StringArray> {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref())
            .ok_or_else(|| anyhow::anyhow!("bad or missing string column `{name}`"))
    };
    let uints = |name: &str| -> anyhow::Result<&UInt32Array> {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref())
            .ok_or_else(|| anyhow::anyhow!("bad or missing uint column `{name}`"))
    };

    let ids = strings("id")?;
    let file_paths = strings("file_path")?;
    let contracts = strings("contract_name")?;
    let functions = strings("function_name")?;
    let code = strings("code_content")?;
    let kinds = strings("kind")?;
    let start_lines = uints("start_line")?;
    let end_lines = uints("end_line")?;

    let vectors: &FixedSizeListArray = batch
        .column_by_name("vector")
        .and_then(|c| c.as_any().downcast_ref())
        .ok_or_else(|| anyhow::anyhow!("bad or missing vector column"))?;
    let symbols: &ListArray = batch
        .column_by_name("referenced_symbols")
        .and_then(|c| c.as_any().downcast_ref())
        .ok_or_else(|| anyhow::anyhow!("bad or missing referenced_symbols column"))?;

    let mut rows = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let vector_values = vectors.value(row);
        let vector_values: &Float32Array = vector_values
            .as_any()
            .downcast_ref()
            .ok_or_else(|| anyhow::anyhow!("bad vector value at row {row}"))?;
        let vector = (0..vector_values.len())
            .map(|idx| vector_values.value(idx))
            .collect();

        let symbol_values = symbols.value(row);
        let symbol_values: &StringArray = symbol_values
            .as_any()
            .downcast_ref()
            .ok_or_else(|| anyhow::anyhow!("bad referenced_symbols value at row {row}"))?;
        let referenced_symbols = symbol_values.iter().flatten().map(str::to_owned).collect();

        rows.push(AnalysisRow {
            id: ids.value(row).to_owned(),
            vector,
            file_path: file_paths.value(row).to_owned(),
            contract_name: contracts.value(row).to_owned(),
            function_name: functions.value(row).to_owned(),
            code_content: code.value(row).to_owned(),
            referenced_symbols,
            kind: kinds.value(row).to_owned(),
            start_line: start_lines.value(row),
            end_line: end_lines.value(row),
        });
    }
    Ok(rows)
}
