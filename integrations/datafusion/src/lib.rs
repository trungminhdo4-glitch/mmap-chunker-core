//! DataFusion integration for mmap-chunker range plans.
//!
//! mmap-chunker plans record-aligned byte ranges; DataFusion executes
//! scans in partitions. This crate joins the two: every planned range
//! becomes exactly one DataFusion execution partition
//! ([`PartitionedFile::with_range`]), so no NDJSON record is split across
//! partitions and every partition is independently schedulable.
//!
//! The integration consumes the versioned plan manifest produced by
//! `mmap-chunker plan` (schema `mmap-chunker-plan`) and uses the core
//! crate's zero-dependency JSON reader, keeping the core dependency-free.
//!
//! ```no_run
//! use std::sync::Arc;
//! use datafusion::physical_plan::ExecutionPlanProperties;
//! use datafusion::prelude::SessionContext;
//! use mmap_chunker_datafusion::NdjsonRangeScan;
//!
//! # async fn run() -> datafusion::error::Result<()> {
//! let schema = mmap_chunker_datafusion::ndjson_schema();
//! let scan = NdjsonRangeScan::from_manifest("huge.jsonl.plan.json", schema)?;
//! let context = SessionContext::new();
//! assert_eq!(scan.execution_plan()?.output_partitioning().partition_count(), scan.ranges.len());
//! let batches = scan.collect(&context).await?;
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::object_store::ObjectStoreUrl;
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::{collect, ExecutionPlan};
use datafusion::prelude::SessionContext;
use datafusion_datasource::file_groups::FileGroup;
use datafusion_datasource::file_scan_config::FileScanConfigBuilder;
use datafusion_datasource::source::DataSourceExec;
use datafusion_datasource::PartitionedFile;
use datafusion_datasource_json::source::JsonSource;
use mmap_chunker_core::json::Json;

/// Manifest schema name accepted by [`load_manifest_ranges`].
pub const PLAN_SCHEMA: &str = "mmap-chunker-plan";

/// The range subset of a plan manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRanges {
    /// Source path recorded in the manifest.
    pub source_path: PathBuf,
    /// Source size in bytes.
    pub source_size: u64,
    /// Record delimiter as hex (delimiter framing only).
    pub delimiter_hex: String,
    /// Record-aligned `(start, end)` byte ranges in manifest order.
    pub ranges: Vec<(usize, usize)>,
}

/// Build an NDJSON schema with an `id: Int64` and `payload: Utf8` column.
///
/// Tests and examples use this; production callers normally pass their
/// own [`SchemaRef`].
pub fn ndjson_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("payload", DataType::Utf8, true),
    ]))
}

fn plan_error(message: impl Into<String>) -> DataFusionError {
    DataFusionError::Plan(message.into())
}

/// Load `(start, end)` ranges from a `mmap-chunker plan` manifest.
///
/// Only delimiter framing manifests are accepted; other framings still
/// describe ranges but need format-specific readers on the DataFusion
/// side.
pub fn load_manifest_ranges(manifest_path: impl AsRef<Path>) -> Result<ManifestRanges> {
    let path = manifest_path.as_ref();
    let document = std::fs::read_to_string(path)
        .map_err(|error| DataFusionError::External(Box::new(error)))?;
    let root = Json::parse(&document).map_err(|error| plan_error(error.to_string()))?;

    match root.get("schema").and_then(Json::as_str) {
        Some(PLAN_SCHEMA) => {}
        other => return Err(plan_error(format!("unexpected manifest schema: {other:?}"))),
    }

    let source = root
        .get("source")
        .ok_or_else(|| plan_error("manifest is missing source"))?;
    let source_path = source
        .get("path")
        .and_then(Json::as_str)
        .ok_or_else(|| plan_error("manifest source path is missing"))?;
    let source_size = source
        .get("size")
        .and_then(Json::as_u64)
        .ok_or_else(|| plan_error("manifest source size is missing"))?;

    let framing = root
        .get("framing")
        .ok_or_else(|| plan_error("manifest is missing framing"))?;
    let delimiter_hex = framing
        .get("delimiter_hex")
        .and_then(Json::as_str)
        .ok_or_else(|| plan_error("only delimiter framing manifests are supported by this reader"))?
        .to_owned();

    let raw_ranges = root
        .get("ranges")
        .and_then(Json::as_array)
        .ok_or_else(|| plan_error("manifest is missing ranges"))?;
    let mut ranges = Vec::with_capacity(raw_ranges.len());
    for (position, entry) in raw_ranges.iter().enumerate() {
        let start = entry
            .get("start")
            .and_then(Json::as_u64)
            .ok_or_else(|| plan_error(format!("range {position} has no start")))?;
        let end = entry
            .get("end")
            .and_then(Json::as_u64)
            .ok_or_else(|| plan_error(format!("range {position} has no end")))?;
        let start = usize::try_from(start)
            .map_err(|_| plan_error(format!("range {position} start exceeds usize")))?;
        let end = usize::try_from(end)
            .map_err(|_| plan_error(format!("range {position} end exceeds usize")))?;
        ranges.push((start, end));
    }

    Ok(ManifestRanges {
        source_path: PathBuf::from(source_path),
        source_size,
        delimiter_hex,
        ranges,
    })
}

/// An NDJSON scan whose partitions are mmap-chunker record-aligned ranges.
#[derive(Debug, Clone)]
pub struct NdjsonRangeScan {
    /// Object-store-relative path of the JSON file.
    pub location: String,
    /// Total file size in bytes.
    pub file_size: u64,
    /// One `(start, end)` range per execution partition.
    pub ranges: Vec<(usize, usize)>,
    /// Schema of the JSON records.
    pub schema: SchemaRef,
}

impl NdjsonRangeScan {
    /// Create a scan from explicit ranges.
    pub fn new(
        location: impl Into<String>,
        file_size: u64,
        ranges: Vec<(usize, usize)>,
        schema: SchemaRef,
    ) -> Self {
        Self {
            location: location.into(),
            file_size,
            ranges,
            schema,
        }
    }

    /// Create a scan from a manifest file and a JSON schema.
    pub fn from_manifest(manifest_path: impl AsRef<Path>, schema: SchemaRef) -> Result<Self> {
        let manifest = load_manifest_ranges(manifest_path)?;
        let location = local_object_location(&manifest.source_path)?;
        Ok(Self::new(
            location,
            manifest.source_size,
            manifest.ranges,
            schema,
        ))
    }

    /// Build the DataFusion execution plan: one file group per range.
    pub fn execution_plan(&self) -> Result<Arc<dyn ExecutionPlan>> {
        let source: Arc<dyn datafusion_datasource::file::FileSource> =
            Arc::new(JsonSource::new(Arc::clone(&self.schema)));
        let groups: Vec<FileGroup> = self
            .ranges
            .iter()
            .map(|(start, end)| {
                FileGroup::new(vec![PartitionedFile::new(
                    self.location.clone(),
                    self.file_size,
                )
                .with_range(*start as i64, *end as i64)])
            })
            .collect();

        let config = FileScanConfigBuilder::new(ObjectStoreUrl::local_filesystem(), source)
            .with_file_groups(groups)
            .build();
        Ok(DataSourceExec::from_data_source(config))
    }

    /// Execute the scan and return all record batches.
    pub async fn collect(&self, context: &SessionContext) -> Result<Vec<RecordBatch>> {
        let plan = self.execution_plan()?;
        collect(plan, context.task_ctx()).await
    }
}

/// Convert a filesystem path into an object-store path relative to the
/// local filesystem root.
pub fn local_object_location(path: &Path) -> Result<String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| DataFusionError::External(Box::new(error)))?;
    let text = canonical
        .to_str()
        .ok_or_else(|| plan_error("source path is not valid UTF-8"))?;
    Ok(text.trim_start_matches(['/', '\\']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_ranges_reject_wrong_schema() {
        let dir = std::env::temp_dir().join("mmap_chunker_df_manifest");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plan.json");
        std::fs::write(&path, r#"{"schema":"other"}"#).unwrap();
        assert!(load_manifest_ranges(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
