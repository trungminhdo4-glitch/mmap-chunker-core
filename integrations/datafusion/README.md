# mmap-chunker × DataFusion

A standalone integration crate that turns `mmap-chunker plan` manifests
into DataFusion execution partitions. The core crate stays dependency-free;
DataFusion and its transitive dependencies live only here.

## What it does

`mmap-chunker` plans record-aligned byte ranges. DataFusion executes file
scans in partitions, one `PartitionedFile` range per partition. This crate
maps each planned range to exactly one execution partition:

```text
mmap-chunker plan huge.jsonl --parts 32
        │
        ▼  huge.jsonl.plan.json  (32 record-aligned ranges)
        │
NdjsonRangeScan::from_manifest(...)
        │  FileScanConfig { file_groups: [range 0, range 1, ..., range 31] }
        ▼
DataSourceExec::from_data_source(config)
        │
        ▼  32 execution partitions, no record split, independently schedulable
```

Each range is parsed by DataFusion's JSON source (`JsonSource`) directly from
the byte range, so no per-partition temporary files are created. Because the
ranges come from `mmap-chunker`, every partition boundary is an exact record
boundary — this is the property DataFusion cannot guarantee when it splits a
file at arbitrary byte offsets.

## Quick start

```sh
# Plan once with the core CLI.
mmap-chunker plan huge.jsonl --parts 32 --output huge.jsonl.plan.json
```

```rust
use std::sync::Arc;
use datafusion::prelude::SessionContext;
use mmap_chunker_datafusion::{ndjson_schema, NdjsonRangeScan};

let scan = NdjsonRangeScan::from_manifest("huge.jsonl.plan.json", ndjson_schema())?;
let context = SessionContext::new();

let plan = scan.execution_plan()?;
assert_eq!(plan.output_partitioning().partition_count(), scan.ranges.len());

let batches = scan.collect(&context).await?;
```

Callers with their own record schema pass a `SchemaRef` instead of
`ndjson_schema()`.

## Scope

- Delimiter-framed NDJSON manifests (`framing.strategy = "delimiter"`).
  Fixed-width and length-prefixed manifests describe ranges too, but need a
  matching DataFusion reader; this crate rejects them explicitly.
- The manifest reader uses the core crate's zero-dependency JSON parser, so
  the integration does not add a second serialization stack.

## Verification

The tests plan ranges with the core crate, run them through DataFusion, and
compare partitioned scans against a single-partition reference scan:

```sh
cargo test
```

Heavy builds run on the Netcup Docker runner instead of the workstation:

```sh
python ../../tools/remote_verify.py --dir integrations/datafusion -- cargo test
```
