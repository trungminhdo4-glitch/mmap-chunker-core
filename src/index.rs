//! Persistent sparse record indexes (sidecar artifacts).
//!
//! Scanning a very large dataset to find record boundaries is the
//! expensive part of planning. A [`RecordIndex`] records every
//! `stride`-th record start once, together with the file identity and
//! the framing used. Later plans can then be derived from indexed
//! boundaries without rescanning the source.
//!
//! The index is a deterministic JSON artifact:
//!
//! ```json
//! {
//!   "schema": "mmap-chunker-index",
//!   "schema_version": 1,
//!   "generator": { "name": "mmap-chunker-core", "version": "0.2.2" },
//!   "source": {
//!     "path": "huge.jsonl",
//!     "size": 123,
//!     "identity": { "sample_fingerprint": "fnv1a64:0x..." }
//!   },
//!   "framing": { "strategy": "delimiter", "delimiter_hex": "0d0a", "delimiter_len": 2 },
//!   "index": { "stride": 10000, "record_count": 523487, "record_offsets": [0, 812431, ...] }
//! }
//! ```
//!
//! Indexed planning uses record-count balancing: requested cut `i` of
//! `N` maps to record `floor(record_count * i / N)`, rounded to the
//! nearest recorded slot. Every cut is therefore an exact record start.

use std::io;
use std::path::Path;

use crate::framing::{FramingDescriptor, FramingStrategy};
use crate::json::Json;
use crate::manifest::{
    file_identity_from_json, identify_file, push_json_string, write_framing_json, FileIdentity,
};
use crate::scanner;
use crate::source::{
    ByteSource, MmapSource, PlannerOptions, PreadSource, SourceMode, WindowedMmapSource,
};

/// Index schema name.
pub const INDEX_SCHEMA: &str = "mmap-chunker-index";

/// Index schema version.
pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// A sparse record index over one immutable file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordIndex {
    /// Source path recorded at build time.
    pub source_path: String,
    /// Source size in bytes.
    pub source_size: u64,
    /// Identity captured before and verified after indexing.
    pub identity: FileIdentity,
    /// Framing used to find record boundaries.
    pub framing: FramingDescriptor,
    /// Every `stride`-th record start is recorded.
    pub stride: u64,
    /// Exact record count observed while scanning.
    pub record_count: u64,
    /// Record starts at indexes `0, stride, 2*stride, ...`.
    pub offsets: Vec<u64>,
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn scan_record_starts(
    source: &dyn ByteSource,
    strategy: &dyn FramingStrategy,
    stride: u64,
    scan_buffer_bytes: usize,
) -> io::Result<(u64, Vec<u64>)> {
    let file_len = source.len();
    let mut offsets = Vec::new();
    if file_len == 0 {
        return Ok((0, offsets));
    }

    let buffer_len = scan_buffer_bytes
        .max(strategy.minimum_buffer_bytes())
        .max(1);
    let mut buffer = vec![0u8; buffer_len];
    let mut boundary_scanner = strategy.scanner(source, &mut buffer);

    let mut cursor = 0usize;
    let mut record_count: u64 = 0;

    loop {
        if record_count % stride == 0 {
            offsets.push(cursor as u64);
        }

        match boundary_scanner.boundary_after(cursor)? {
            Some(end) => {
                if end <= cursor {
                    return Err(invalid_data(
                        "framing boundary did not advance; index aborted",
                    ));
                }
                record_count += 1;
                if end >= file_len {
                    break;
                }
                cursor = end;
            }
            None => {
                // The file remainder is the final record.
                record_count += 1;
                break;
            }
        }
    }

    Ok((record_count, offsets))
}

/// Build a sparse record index for `path`.
///
/// The file is scanned once; identity is captured before and re-checked
/// after the scan, so an index can never describe mixed content.
///
/// # Safety
///
/// The caller must ensure that the file is not modified while this
/// function runs (mmap-backed modes; see
/// [`plan_partition_ranges`](crate::source::plan_partition_ranges)).
pub unsafe fn build_record_index(
    path: impl AsRef<Path>,
    strategy: &dyn FramingStrategy,
    stride: u64,
    options: &PlannerOptions,
) -> io::Result<RecordIndex> {
    if stride == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stride must be > 0",
        ));
    }

    let path = path.as_ref();
    let identity_before = identify_file(path)?;
    let buffer = options.scan_buffer_bytes.max(1);

    let (record_count, offsets) = match options.mode {
        SourceMode::Mmap => {
            let source = unsafe { MmapSource::open_path(path)? };
            scan_record_starts(&source, strategy, stride, buffer)?
        }
        SourceMode::Windowed => {
            let source = unsafe { WindowedMmapSource::open_path(path, options.window_bytes)? };
            scan_record_starts(&source, strategy, stride, buffer)?
        }
        SourceMode::Pread => {
            let source = PreadSource::open_path(path)?;
            scan_record_starts(&source, strategy, stride, buffer)?
        }
    };

    let identity_after = identify_file(path)?;
    if identity_before != identity_after {
        return Err(io::Error::other(
            "source file changed while indexing; index rejected",
        ));
    }

    Ok(RecordIndex {
        source_path: path.to_string_lossy().into_owned(),
        source_size: identity_before.size,
        identity: identity_before,
        framing: strategy.describe(),
        stride,
        record_count,
        offsets,
    })
}

/// Derive record-aligned partition boundaries from an index.
///
/// Uses record-count balancing: cut `i` of `N` maps to record
/// `floor(record_count * i / N)` rounded to the nearest recorded slot.
/// Returns an error if `file_len` does not match the indexed size or
/// the index is structurally invalid.
pub fn plan_partition_boundaries_from_index(
    index: &RecordIndex,
    num_partitions: usize,
    file_len: usize,
) -> io::Result<Vec<(usize, usize)>> {
    if index.stride == 0 {
        return Err(invalid_data("index stride must be > 0"));
    }
    if file_len as u64 != index.source_size {
        return Err(invalid_data(format!(
            "file size {} does not match indexed size {}",
            file_len, index.source_size
        )));
    }
    if index.record_count > 0 && index.offsets.first() != Some(&0) {
        return Err(invalid_data("index offsets must start at 0"));
    }
    if index.offsets.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid_data("index offsets must be strictly increasing"));
    }
    if index
        .offsets
        .iter()
        .any(|&offset| offset >= index.source_size.max(1))
    {
        return Err(invalid_data("index offset is outside the source"));
    }

    if file_len == 0 || num_partitions == 0 || index.record_count == 0 {
        return Ok(Vec::new());
    }
    if num_partitions == 1 {
        return Ok(vec![(0, file_len)]);
    }
    if index.offsets.is_empty() {
        return Err(invalid_data("non-empty index must contain offsets"));
    }

    let n = num_partitions.min(file_len);
    let stride = index.stride as u128;
    let mut boundaries = Vec::new();
    let mut last_boundary: usize = 0;

    for i in 1..n {
        // Record-count balancing, rounded to the nearest recorded slot.
        let target_record = u128::from(index.record_count) * i as u128 / n as u128;
        let slot = usize::try_from((target_record + stride / 2) / stride)
            .unwrap_or(usize::MAX)
            .min(index.offsets.len() - 1);
        let boundary = usize::try_from(index.offsets[slot])
            .unwrap_or(usize::MAX)
            .min(file_len);
        if boundary == 0 || boundary <= last_boundary || boundary >= file_len {
            continue;
        }
        boundaries.push(boundary);
        last_boundary = boundary;
    }

    Ok(scanner::ranges_from_boundaries(file_len, &boundaries))
}

impl RecordIndex {
    /// Render the index as deterministic, pretty-printed JSON.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str("  \"schema\": ");
        push_json_string(&mut out, INDEX_SCHEMA);
        out.push_str(",\n");
        out.push_str(&format!("  \"schema_version\": {INDEX_SCHEMA_VERSION},\n"));
        out.push_str("  \"generator\": { \"name\": \"mmap-chunker-core\", \"version\": ");
        push_json_string(&mut out, env!("CARGO_PKG_VERSION"));
        out.push_str(" },\n");

        out.push_str("  \"source\": {\n");
        out.push_str("    \"path\": ");
        push_json_string(&mut out, &self.source_path);
        out.push_str(",\n");
        out.push_str(&format!("    \"size\": {},\n", self.source_size));
        out.push_str("    \"identity\": ");
        self.identity.write_json(&mut out, "    ");
        out.push('\n');
        out.push_str("  },\n");

        out.push_str("  \"framing\": ");
        write_framing_json(&mut out, &self.framing, "  ");
        out.push_str(",\n");

        out.push_str("  \"index\": {\n");
        out.push_str(&format!("    \"stride\": {},\n", self.stride));
        out.push_str(&format!("    \"record_count\": {},\n", self.record_count));
        out.push_str("    \"record_offsets\": [");
        if self.offsets.is_empty() {
            out.push_str("]\n");
        } else {
            out.push('\n');
            for (index, offset) in self.offsets.iter().enumerate() {
                out.push_str(&format!("      {offset}"));
                if index + 1 < self.offsets.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str("    ]\n");
        }
        out.push_str("  }\n}");
        out
    }

    /// Write [`to_json`](Self::to_json) to `path`.
    pub fn write_json(&self, path: impl AsRef<Path>) -> io::Result<()> {
        std::fs::write(path, self.to_json())
    }

    /// Parse an index document, validating schema and structure.
    pub fn parse(text: &str) -> io::Result<Self> {
        let root = Json::parse(text).map_err(|error| invalid_data(error.to_string()))?;

        match root.get("schema").and_then(Json::as_str) {
            Some(schema) if schema == INDEX_SCHEMA => {}
            _ => return Err(invalid_data("unexpected index schema")),
        }
        match root.get("schema_version").and_then(Json::as_u64) {
            Some(version) if version == u64::from(INDEX_SCHEMA_VERSION) => {}
            _ => return Err(invalid_data("unsupported index schema_version")),
        }

        let source = root
            .get("source")
            .ok_or_else(|| invalid_data("index is missing source"))?;
        let identity = file_identity_from_json(source)
            .ok_or_else(|| invalid_data("index source identity is malformed"))?;
        let source_path = source
            .get("path")
            .and_then(Json::as_str)
            .ok_or_else(|| invalid_data("index source path is missing"))?
            .to_owned();

        let framing = crate::manifest::framing_descriptor_from_json(
            root.get("framing")
                .ok_or_else(|| invalid_data("index is missing framing"))?,
        )
        .ok_or_else(|| invalid_data("index framing descriptor is malformed"))?;

        let index = root
            .get("index")
            .ok_or_else(|| invalid_data("index is missing index block"))?;
        let stride = index
            .get("stride")
            .and_then(Json::as_u64)
            .ok_or_else(|| invalid_data("index stride is missing"))?;
        if stride == 0 {
            return Err(invalid_data("index stride must be > 0"));
        }
        let record_count = index
            .get("record_count")
            .and_then(Json::as_u64)
            .ok_or_else(|| invalid_data("index record_count is missing"))?;
        let raw_offsets = index
            .get("record_offsets")
            .and_then(Json::as_array)
            .ok_or_else(|| invalid_data("index record_offsets is missing"))?;

        let mut offsets = Vec::with_capacity(raw_offsets.len());
        for value in raw_offsets {
            offsets.push(
                value
                    .as_u64()
                    .ok_or_else(|| invalid_data("index offset is not an unsigned integer"))?,
            );
        }
        if record_count > 0 && offsets.first() != Some(&0) {
            return Err(invalid_data("index offsets must start at 0"));
        }
        if offsets.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid_data("index offsets must be strictly increasing"));
        }
        if offsets.iter().any(|&offset| offset >= identity.size.max(1)) {
            return Err(invalid_data("index offset is outside the source"));
        }

        Ok(Self {
            source_path,
            source_size: identity.size,
            identity,
            framing,
            stride,
            record_count,
            offsets,
        })
    }

    /// Load an index file.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::BuiltinFraming;
    use crate::manifest::plan_from_ranges;

    fn temp_file(label: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mmap_chunker_index_{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn cleanup(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    fn crlf_records(count: usize) -> Vec<u8> {
        let mut content = Vec::new();
        for index in 0..count {
            content.extend_from_slice(format!("record-{index:04}\r\n").as_bytes());
        }
        content
    }

    #[test]
    fn builds_index_for_delimiter_records() {
        let content = crlf_records(10);
        let path = temp_file("delimiter", &content);
        let strategy = BuiltinFraming::delimiter(b"\r\n".to_vec()).unwrap();
        let options = PlannerOptions::default();

        let index = unsafe { build_record_index(&path, &strategy, 3, &options) }.unwrap();
        assert_eq!(index.record_count, 10);
        assert_eq!(index.stride, 3);
        assert_eq!(index.source_size, content.len() as u64);
        // Records are 13 bytes long; every third record start is recorded.
        assert_eq!(index.offsets, vec![0, 39, 78, 117]);
        assert_eq!(index.identity, identify_file(&path).unwrap());

        cleanup(&path);
    }

    #[test]
    fn builds_index_for_fixed_width_and_pread() {
        let content: Vec<u8> = (0..100u8).collect();
        let path = temp_file("fixed", &content);
        let strategy = BuiltinFraming::fixed_width(16).unwrap();
        let options = PlannerOptions::new(SourceMode::Pread);

        let index = unsafe { build_record_index(&path, &strategy, 2, &options) }.unwrap();
        assert_eq!(index.record_count, 7);
        assert_eq!(index.offsets, vec![0, 32, 64, 96]);

        cleanup(&path);
    }

    #[test]
    fn builds_index_for_length_prefixed_records() {
        let payloads: &[&[u8]] = &[b"alpha", b"be", b"gamma-payload", b""];
        let mut content = Vec::new();
        for payload in payloads {
            content.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            content.extend_from_slice(payload);
        }
        let path = temp_file("length", &content);
        let strategy = BuiltinFraming::length_prefixed(2, true, false).unwrap();
        let options = PlannerOptions::new(SourceMode::Windowed)
            .with_window_bytes(crate::source::MIN_WINDOW_BYTES);

        let index = unsafe { build_record_index(&path, &strategy, 2, &options) }.unwrap();
        assert_eq!(index.record_count, 4);
        assert_eq!(index.offsets, vec![0, 11]);

        cleanup(&path);
    }

    #[test]
    fn index_json_round_trips() {
        let content = crlf_records(7);
        let path = temp_file("roundtrip", &content);
        let strategy = BuiltinFraming::delimiter(b"\r\n".to_vec()).unwrap();
        let index =
            unsafe { build_record_index(&path, &strategy, 2, &PlannerOptions::default()) }.unwrap();

        let json = index.to_json();
        assert!(json.contains("\"schema\": \"mmap-chunker-index\""));
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"stride\": 2"));
        assert!(json.contains("\"record_count\": 7"));
        assert!(json.contains("\"delimiter_hex\": \"0d0a\""));

        let parsed = RecordIndex::parse(&json).unwrap();
        assert_eq!(parsed, index);
        assert_eq!(parsed.to_json(), json);

        cleanup(&path);
    }

    #[test]
    fn indexed_planning_produces_record_aligned_ranges() {
        let content = crlf_records(100);
        let path = temp_file("plan_index", &content);
        let strategy = BuiltinFraming::delimiter(b"\r\n".to_vec()).unwrap();
        let index =
            unsafe { build_record_index(&path, &strategy, 4, &PlannerOptions::default()) }.unwrap();

        let ranges = plan_partition_boundaries_from_index(&index, 8, content.len()).unwrap();
        assert!(ranges.len() <= 8);
        let mut cursor = 0usize;
        for (start, end) in &ranges {
            assert_eq!(*start, cursor);
            assert!(end > start);
            assert!(
                index.offsets.contains(&(*start as u64)),
                "range start {start} is not an indexed record start"
            );
            cursor = *end;
        }
        assert_eq!(cursor, content.len());

        // Every non-final range ends on a complete CRLF record.
        for (start, end) in ranges.iter().take(ranges.len() - 1) {
            assert_eq!(&content[*end - 2..*end], b"\r\n", "range {start}..{end}");
        }

        // An index-derived manifest seals identity and records the index.
        let plan = plan_from_ranges(
            &path,
            &index.identity,
            index.framing.clone(),
            8,
            crate::manifest::IndexReference {
                stride: index.stride,
                record_count: index.record_count,
                path: Some("data.bin.mmapidx".to_owned()),
            },
            ranges.clone(),
        )
        .unwrap();
        assert_eq!(plan.ranges, ranges);
        let json = plan.to_json();
        assert!(json.contains("\"strategy\": \"indexed_records\""));
        assert!(json.contains("\"source_mode\": null"));
        assert!(json.contains("\"stride\": 4"));

        // A stale identity must be rejected.
        let mut stale = index.identity.clone();
        stale.sample_fingerprint = Some("fnv1a64:0x0000000000000000".to_owned());
        assert!(plan_from_ranges(
            &path,
            &stale,
            index.framing.clone(),
            8,
            crate::manifest::IndexReference {
                stride: index.stride,
                record_count: index.record_count,
                path: None,
            },
            ranges,
        )
        .is_err());

        cleanup(&path);
    }

    #[test]
    fn indexed_planning_rejects_mismatched_file_size() {
        let content = crlf_records(5);
        let path = temp_file("mismatch", &content);
        let strategy = BuiltinFraming::delimiter(b"\r\n".to_vec()).unwrap();
        let index =
            unsafe { build_record_index(&path, &strategy, 1, &PlannerOptions::default()) }.unwrap();

        assert!(plan_partition_boundaries_from_index(&index, 4, content.len() + 1).is_err());
        cleanup(&path);
    }

    #[test]
    fn parse_rejects_malformed_indexes() {
        let cases = [
            "{}",
            r#"{"schema": "mmap-chunker-index"}"#,
            r#"{"schema": "mmap-chunker-index", "schema_version": 2}"#,
            r#"{"schema": "mmap-chunker-index", "schema_version": 1, "source": {"path": "x", "size": 10, "identity": {"sample_bytes": 1, "sample_fingerprint": null, "modified_unix_nanos": null, "device": null, "inode": null}}, "framing": {"strategy": "delimiter", "delimiter_hex": "0a", "delimiter_len": 1}, "index": {"stride": 1, "record_count": 2, "record_offsets": [1, 2]}}"#,
            r#"{"schema": "mmap-chunker-index", "schema_version": 1, "source": {"path": "x", "size": 10, "identity": {"sample_bytes": 1, "sample_fingerprint": null, "modified_unix_nanos": null, "device": null, "inode": null}}, "framing": {"strategy": "delimiter", "delimiter_hex": "0a", "delimiter_len": 1}, "index": {"stride": 1, "record_count": 2, "record_offsets": [0, 0]}}"#,
        ];
        for case in cases {
            assert!(RecordIndex::parse(case).is_err(), "case parsed: {case}");
        }
    }

    #[test]
    fn empty_file_builds_empty_index() {
        let path = temp_file("empty", b"");
        let strategy = BuiltinFraming::delimiter(b"\n".to_vec()).unwrap();
        let index =
            unsafe { build_record_index(&path, &strategy, 1, &PlannerOptions::default()) }.unwrap();
        assert_eq!(index.record_count, 0);
        assert!(index.offsets.is_empty());
        assert!(plan_partition_boundaries_from_index(&index, 4, 0)
            .unwrap()
            .is_empty());
        cleanup(&path);
    }
}
