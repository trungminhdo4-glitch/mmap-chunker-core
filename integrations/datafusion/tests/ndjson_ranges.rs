use std::path::Path;

use datafusion::arrow::array::Int64Array;
use datafusion::physical_plan::ExecutionPlanProperties;
use datafusion::prelude::SessionContext;
use mmap_chunker_core::{
    plan_file_with_framing, plan_partition_ranges_with, BuiltinFraming, PlannerOptions, SourceMode,
};
use mmap_chunker_datafusion::{local_object_location, ndjson_schema, NdjsonRangeScan};

fn write_fixture(dir: &Path, records: usize) -> std::path::PathBuf {
    let mut content = String::new();
    for index in 0..records {
        let payload = "x".repeat((index * 7) % 41);
        content.push_str(&format!("{{\"id\":{index},\"payload\":\"{payload}\"}}\n"));
    }
    let path = dir.join("records.jsonl");
    std::fs::write(&path, content).unwrap();
    path
}

async fn scan_aggregate(scan: &NdjsonRangeScan) -> (i64, i64) {
    let context = SessionContext::new();
    let batches = scan.collect(&context).await.unwrap();
    let mut rows = 0i64;
    let mut sum = 0i64;
    for batch in batches {
        rows += i64::try_from(batch.num_rows()).unwrap();
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        for value in ids.iter().flatten() {
            sum += value;
        }
    }
    (rows, sum)
}

fn scan_for(path: &Path, ranges: Vec<(usize, usize)>) -> NdjsonRangeScan {
    NdjsonRangeScan::new(
        local_object_location(path).unwrap(),
        std::fs::metadata(path).unwrap().len(),
        ranges,
        ndjson_schema(),
    )
}

#[tokio::test]
async fn planned_ranges_become_execution_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_fixture(dir.path(), 1000);
    let strategy = BuiltinFraming::delimiter(b"\n".to_vec()).unwrap();
    let ranges = unsafe {
        plan_partition_ranges_with(&path, 8, &strategy, &PlannerOptions::new(SourceMode::Mmap))
    }
    .unwrap();
    assert_eq!(ranges.len(), 8, "expected 8 record-aligned ranges");

    let scan = scan_for(&path, ranges.clone());
    let plan = scan.execution_plan().unwrap();
    assert_eq!(
        plan.output_partitioning().partition_count(),
        ranges.len(),
        "one execution partition per planned range"
    );

    let (rows, sum) = scan_aggregate(&scan).await;
    assert_eq!(rows, 1000);
    assert_eq!(sum, 1000 * 999 / 2);

    let reference = scan_for(&path, vec![(0, scan.file_size as usize)]);
    let reference_plan = reference.execution_plan().unwrap();
    assert_eq!(reference_plan.output_partitioning().partition_count(), 1);
    let (reference_rows, reference_sum) = scan_aggregate(&reference).await;
    assert_eq!(
        (rows, sum),
        (reference_rows, reference_sum),
        "partitioned scan must match the single-partition scan"
    );
}

#[tokio::test]
async fn every_partition_boundary_is_a_record_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let records = 2001;
    let path = write_fixture(dir.path(), records);
    let content = std::fs::read(&path).unwrap();
    let strategy = BuiltinFraming::delimiter(b"\n".to_vec()).unwrap();
    let ranges = unsafe {
        plan_partition_ranges_with(&path, 13, &strategy, &PlannerOptions::new(SourceMode::Mmap))
    }
    .unwrap();
    assert_eq!(ranges.len(), 13);
    for &(start, end) in &ranges[..ranges.len() - 1] {
        assert_eq!(
            content[end - 1],
            b'\n',
            "range {start}..{end} splits a record"
        );
    }

    let scan = scan_for(&path, ranges);
    let (rows, sum) = scan_aggregate(&scan).await;
    assert_eq!(rows, records as i64);
    assert_eq!(sum, (records as i64) * (records as i64 - 1) / 2);
}

#[tokio::test]
async fn windowed_plan_and_manifest_drive_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let records = 777;
    let path = write_fixture(dir.path(), records);
    let strategy = BuiltinFraming::delimiter(b"\n".to_vec()).unwrap();

    let plan = unsafe {
        plan_file_with_framing(
            &path,
            records / 100,
            &strategy,
            SourceMode::Windowed,
            65536,
        )
    }
    .unwrap();
    let manifest_path = dir.path().join("records.jsonl.plan.json");
    plan.write_json(&manifest_path).unwrap();

    let scan = NdjsonRangeScan::from_manifest(&manifest_path, ndjson_schema()).unwrap();
    assert_eq!(scan.ranges, plan.ranges);
    assert_eq!(scan.file_size, plan.source_size);

    let plan_partitions = scan.execution_plan().unwrap();
    assert_eq!(
        plan_partitions.output_partitioning().partition_count(),
        plan.ranges.len()
    );

    let (rows, sum) = scan_aggregate(&scan).await;
    assert_eq!(rows, records as i64);
    assert_eq!(sum, (records as i64) * (records as i64 - 1) / 2);
}
