use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use mmap_chunker_core::ffi::{
    mmap_engine_free, mmap_engine_get_chunk, mmap_engine_open, mmap_engine_partition_records,
    mmap_engine_scan_chunks_ex, mmap_engine_scan_chunks_pattern, mmap_engine_scan_fixed,
    CChunkView,
};
use mmap_chunker_core::json::Json;
use mmap_chunker_core::manifest::{
    identify_file, plan_file_with_framing, plan_from_ranges, CoverageKind, IndexReference,
    ManifestErrorKind, RangePlan, MAX_MANIFEST_BYTES, MAX_MANIFEST_RANGES,
};
use mmap_chunker_core::{BuiltinFraming, FramingDescriptor, MmapChunker, SourceMode};

#[derive(Debug, PartialEq, Eq)]
struct ObservedRange {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

enum PlanRequest {
    Single {
        chunk_size: usize,
        delimiter: u8,
    },
    Pattern {
        chunk_size: usize,
        delimiter: Vec<u8>,
    },
    Fixed {
        chunk_size: usize,
    },
    Partition {
        partitions: usize,
        delimiter: u8,
    },
}

fn fixture_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mmap_chunker_core_plan_parity_{}_{}",
        std::process::id(),
        name
    ))
}

fn write_fixture(name: &str, data: &[u8]) -> PathBuf {
    let path = fixture_path(name);
    let _ = fs::remove_file(&path);
    fs::write(&path, data).unwrap();
    path
}

fn rust_ranges(path: &Path, request: &PlanRequest) -> Vec<ObservedRange> {
    let mut chunker = unsafe { MmapChunker::open(path).unwrap() };
    let count = match request {
        PlanRequest::Single {
            chunk_size,
            delimiter,
        } => chunker.scan_delimited(*chunk_size, *delimiter),
        PlanRequest::Pattern {
            chunk_size,
            delimiter,
        } => chunker.scan_delimited_pattern(*chunk_size, delimiter),
        PlanRequest::Fixed { chunk_size } => chunker.scan_fixed(*chunk_size),
        PlanRequest::Partition {
            partitions,
            delimiter,
        } => chunker.partition_records(*partitions, *delimiter),
    };

    let source = chunker.as_bytes();
    let base = source.as_ptr();
    (0..count)
        .map(|index| {
            let chunk = chunker.get_chunk(index).unwrap();
            let start = unsafe { chunk.as_ptr().offset_from(base) as usize };
            let end = start + chunk.len();
            ObservedRange {
                start,
                end,
                bytes: chunk.to_vec(),
            }
        })
        .collect()
}

unsafe fn ffi_ranges(path: &Path, request: &PlanRequest) -> Vec<ObservedRange> {
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    let handle = mmap_engine_open(c_path.as_ptr().cast());
    assert!(!handle.is_null());

    let count = match request {
        PlanRequest::Single {
            chunk_size,
            delimiter,
        } => mmap_engine_scan_chunks_ex(handle, *chunk_size, *delimiter),
        PlanRequest::Pattern {
            chunk_size,
            delimiter,
        } => mmap_engine_scan_chunks_pattern(
            handle,
            *chunk_size,
            delimiter.as_ptr(),
            delimiter.len(),
        ),
        PlanRequest::Fixed { chunk_size } => mmap_engine_scan_fixed(handle, *chunk_size),
        PlanRequest::Partition {
            partitions,
            delimiter,
        } => mmap_engine_partition_records(handle, *partitions, *delimiter),
    };

    let mut views = Vec::with_capacity(count);
    for index in 0..count {
        let mut view = CChunkView {
            data: std::ptr::null(),
            len: 0,
        };
        assert_eq!(mmap_engine_get_chunk(handle, index, &mut view), 0);
        views.push(view);
    }

    let base = views.first().map_or(std::ptr::null(), |view| view.data);
    let ranges = views
        .into_iter()
        .map(|view| {
            let start = view.data.offset_from(base) as usize;
            let end = start + view.len;
            ObservedRange {
                start,
                end,
                bytes: std::slice::from_raw_parts(view.data, view.len).to_vec(),
            }
        })
        .collect();

    mmap_engine_free(handle);
    ranges
}

fn assert_parity(name: &str, data: &[u8], request: PlanRequest) {
    let path = write_fixture(name, data);
    let rust = rust_ranges(&path, &request);
    let ffi = unsafe { ffi_ranges(&path, &request) };

    assert_eq!(rust, ffi, "Rust/FFI range mismatch for fixture {name}");
    for range in rust {
        assert!(range.start <= range.end);
        assert_eq!(range.end - range.start, range.bytes.len());
    }

    fs::remove_file(path).unwrap();
}

#[test]
fn single_byte_plans_have_cross_surface_parity() {
    assert_parity(
        "single_newline",
        b"a\nb\nc\n",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "single_nul",
        b"a\0b\0c\0",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: 0,
        },
    );
    assert_parity(
        "single_absent",
        b"abcdef",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "single_no_trailing",
        b"a\nb\nc",
        PlanRequest::Single {
            chunk_size: 2,
            delimiter: b'\n',
        },
    );
}

#[test]
fn pattern_plans_have_cross_surface_parity() {
    assert_parity(
        "pattern_crlf",
        b"a\r\nb\r\nc",
        PlanRequest::Pattern {
            chunk_size: 2,
            delimiter: b"\r\n".to_vec(),
        },
    );
    assert_parity(
        "pattern_double_crlf",
        b"head\r\n\r\nbody\r\n\r\n",
        PlanRequest::Pattern {
            chunk_size: 4,
            delimiter: b"\r\n\r\n".to_vec(),
        },
    );
    assert_parity(
        "pattern_embedded_nul",
        b"a\0\0b\0\0c",
        PlanRequest::Pattern {
            chunk_size: 2,
            delimiter: b"\0\0".to_vec(),
        },
    );
    assert_parity(
        "pattern_absent",
        b"abcdef",
        PlanRequest::Pattern {
            chunk_size: 2,
            delimiter: b"\r\n".to_vec(),
        },
    );
    assert_parity(
        "pattern_longer_than_data",
        b"abc",
        PlanRequest::Pattern {
            chunk_size: 1,
            delimiter: b"abcdef".to_vec(),
        },
    );
    assert_parity(
        "pattern_empty_file",
        b"",
        PlanRequest::Pattern {
            chunk_size: 4,
            delimiter: b"\r\n".to_vec(),
        },
    );
}

#[test]
fn fixed_plans_have_cross_surface_parity() {
    assert_parity(
        "fixed_exact",
        b"12345678",
        PlanRequest::Fixed { chunk_size: 4 },
    );
    assert_parity(
        "fixed_remainder",
        b"1234567890",
        PlanRequest::Fixed { chunk_size: 4 },
    );
    assert_parity(
        "fixed_zero_size",
        b"12345",
        PlanRequest::Fixed { chunk_size: 0 },
    );
    assert_parity("fixed_empty", b"", PlanRequest::Fixed { chunk_size: 4 });
}

#[test]
fn partition_plans_have_cross_surface_parity() {
    assert_parity(
        "partition_one",
        b"a\nb\nc\n",
        PlanRequest::Partition {
            partitions: 1,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_many",
        b"a\nb\nc\nd\ne\n",
        PlanRequest::Partition {
            partitions: 3,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_giant_record",
        b"giant record without boundary until here\nsmall\n",
        PlanRequest::Partition {
            partitions: 8,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_no_delimiter",
        b"abcdef",
        PlanRequest::Partition {
            partitions: 4,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_fewer_records",
        b"a\nb\n",
        PlanRequest::Partition {
            partitions: 8,
            delimiter: b'\n',
        },
    );
    assert_parity(
        "partition_empty_file",
        b"",
        PlanRequest::Partition {
            partitions: 4,
            delimiter: b'\n',
        },
    );
}

fn delimiter_manifest_plan(path: &Path, partitions: usize) -> RangePlan {
    let strategy = BuiltinFraming::delimiter(b"\n".to_vec()).unwrap();
    unsafe { plan_file_with_framing(path, partitions, &strategy, SourceMode::Mmap, 0) }.unwrap()
}

#[test]
fn manifest_canonical_bytes_carry_per_range_digests() {
    // Small synthetic fixture: deterministic, no large files.
    let data = b"alpha\nbeta\ngamma\ndelta\n";
    let path = write_fixture("manifest_canonical", data);
    let plan = delimiter_manifest_plan(&path, 2);
    assert!(!plan.ranges.is_empty());

    // Deterministic canonicalization of the already-serialized structure.
    let first = plan.canonical_bytes();
    assert_eq!(first, plan.canonical_bytes());
    assert_eq!(
        first,
        plan.to_json().into_bytes(),
        "canonical_bytes must be the canonical form of the serialized manifest"
    );

    // Per-range digests present: every range carries index/start/end/length
    // and each entry round-trips through the crate's own strict reader.
    let text = String::from_utf8(first).unwrap();
    let parsed = Json::parse(&text).unwrap();
    let ranges = parsed.get("ranges").unwrap().as_array().unwrap();
    assert_eq!(
        ranges.len(),
        plan.ranges.len(),
        "every planned range must be present in the canonical manifest"
    );
    for (index, ((start, end), entry)) in plan.ranges.iter().zip(ranges.iter()).enumerate() {
        assert_eq!(
            entry.get("index").unwrap().as_u64().unwrap(),
            index as u64,
            "per-range index digest missing for range {index}"
        );
        assert_eq!(
            entry.get("start").unwrap().as_u64().unwrap(),
            *start as u64,
            "per-range start digest missing for range {index}"
        );
        assert_eq!(
            entry.get("end").unwrap().as_u64().unwrap(),
            *end as u64,
            "per-range end digest missing for range {index}"
        );
        assert_eq!(
            entry.get("length").unwrap().as_u64().unwrap(),
            (*end - *start) as u64,
            "per-range length cross-check failed for range {index}"
        );
    }

    fs::remove_file(path).unwrap();
}

#[test]
fn manifest_verify_coverage_proves_gap_and_overlap_free_ranges() {
    // Six 4-byte records: every valid split lands on a 4-byte boundary,
    // so off-by-one tampering is unambiguous.
    let data = b"r01\nr02\nr03\nr04\nr05\nr06\n";
    let path = write_fixture("manifest_coverage", data);
    let plan = delimiter_manifest_plan(&path, 3);
    assert!(
        plan.ranges.len() >= 2,
        "fixture must yield at least two ranges, got {:?}",
        plan.ranges
    );

    // Coverage proof on the exact planned bytes.
    plan.verify_coverage(data)
        .expect("valid plan must verify gap/overlap-free");

    // Fail-closed: each tampered plan is rejected with a machine-readable kind.
    let mut gap = plan.clone();
    gap.ranges[1].0 += 1;
    assert_eq!(
        gap.verify_coverage(data).unwrap_err().kind,
        CoverageKind::Gap
    );

    let mut overlap = plan.clone();
    overlap.ranges[1].0 -= 1;
    assert_eq!(
        overlap.verify_coverage(data).unwrap_err().kind,
        CoverageKind::Overlap
    );

    let mut bad_start = plan.clone();
    bad_start.ranges[0].0 = 1;
    assert_eq!(
        bad_start.verify_coverage(data).unwrap_err().kind,
        CoverageKind::BadStart
    );

    let mut bad_end = plan.clone();
    let last = bad_end.ranges.len() - 1;
    bad_end.ranges[last].1 -= 1;
    assert_eq!(
        bad_end.verify_coverage(data).unwrap_err().kind,
        CoverageKind::BadEnd
    );

    // Contiguity preserved, but the boundary moved inside a record: the
    // BuiltinFraming BoundaryScanner replay must catch it.
    let mut misaligned = plan.clone();
    misaligned.ranges[0].1 -= 1;
    misaligned.ranges[1].0 = misaligned.ranges[0].1;
    assert_eq!(
        misaligned.verify_coverage(data).unwrap_err().kind,
        CoverageKind::Misaligned
    );

    // Truncated input never verifies against the sealed size.
    assert_eq!(
        plan.verify_coverage(&data[..data.len() - 1])
            .unwrap_err()
            .kind,
        CoverageKind::SizeMismatch
    );

    fs::remove_file(path).unwrap();
}

#[test]
fn manifest_verify_coverage_accepts_empty_file() {
    let path = write_fixture("manifest_empty", b"");
    let plan = delimiter_manifest_plan(&path, 2);
    assert!(plan.ranges.is_empty());
    plan.verify_coverage(b"")
        .expect("empty plan must verify against empty input");
    fs::remove_file(path).unwrap();
}

#[test]
fn manifest_verify_coverage_skips_replay_for_custom_framing() {
    let data = b"abcdefgh";
    let path = write_fixture("manifest_custom", data);
    let identity = identify_file(&path).unwrap();
    let plan = plan_from_ranges(
        &path,
        &identity,
        FramingDescriptor::Custom {
            name: "test-format".to_owned(),
        },
        2,
        IndexReference {
            stride: 1,
            record_count: 2,
            path: None,
        },
        vec![(0, 4), (4, 8)],
    )
    .unwrap();

    // No scanner available: structural proof only, still fail-closed.
    plan.verify_coverage(data)
        .expect("structurally sound custom plan must verify");
    let mut gap = plan.clone();
    gap.ranges[1] = (5, 8);
    assert_eq!(
        gap.verify_coverage(data).unwrap_err().kind,
        CoverageKind::Gap
    );

    fs::remove_file(path).unwrap();
}

#[test]
fn manifest_verify_coverage_rejects_structural_and_framing_violations() {
    // Base: structurally sound delimiter plan; every case below mutates
    // exactly one property so each `CoverageKind` has its own witness.
    let data = b"r01\nr02\nr03\nr04\n";
    let path = write_fixture("manifest_negative", data);
    let plan = delimiter_manifest_plan(&path, 2);
    plan.verify_coverage(data)
        .expect("base plan must verify before tampering");
    assert!(
        plan.ranges.len() >= 2,
        "base fixture must yield at least two ranges, got {:?}",
        plan.ranges
    );

    // Zero-length and inverted ranges are never records.
    let mut zero_length = plan.clone();
    zero_length.ranges[0] = (0, 0);
    assert_eq!(
        zero_length.verify_coverage(data).unwrap_err().kind,
        CoverageKind::InvertedRange
    );
    let mut inverted = plan.clone();
    inverted.ranges[0] = (8, 4);
    assert_eq!(
        inverted.verify_coverage(data).unwrap_err().kind,
        CoverageKind::InvertedRange
    );

    // A later range starting before the previous start is unsorted,
    // even when an overlap reading would also apply.
    let mut unsorted = plan.clone();
    unsorted.ranges = vec![(0, 4), (4, 8), (2, 16)];
    unsorted.source_size = 16;
    let wide = b"r01\nr02\nr03\nr04\n";
    assert_eq!(
        unsorted.verify_coverage(wide).unwrap_err().kind,
        CoverageKind::Unsorted
    );

    // Exact duplicates double-deliver bytes: rejected as overlap.
    let mut duplicate = plan.clone();
    duplicate.ranges = vec![(0, 8), (0, 8)];
    duplicate.source_size = 8;
    assert_eq!(
        duplicate.verify_coverage(&data[..8]).unwrap_err().kind,
        CoverageKind::Overlap
    );

    // An unusable framing descriptor fails closed instead of replaying.
    let mut bad_framing = plan.clone();
    bad_framing.framing = FramingDescriptor::Delimiter {
        delimiter: Vec::new(),
    };
    assert_eq!(
        bad_framing.verify_coverage(data).unwrap_err().kind,
        CoverageKind::InvalidFraming
    );

    // Length-prefixed replay failure: the declared record exceeds the
    // file, so alignment cannot be proven (fail-closed, not truncated).
    let lp_data: Vec<u8> = [0x64, 0x00, 0x00, 0x00]
        .into_iter()
        .chain([0xAA; 8])
        .collect();
    assert_eq!(lp_data.len(), 12);
    let mut oversized = plan.clone();
    oversized.source_size = 12;
    oversized.ranges = vec![(0, 6), (6, 12)];
    oversized.framing = FramingDescriptor::LengthPrefixed {
        prefix_bytes: 4,
        little_endian: true,
        length_includes_prefix: false,
    };
    assert_eq!(
        oversized.verify_coverage(&lp_data).unwrap_err().kind,
        CoverageKind::Boundary
    );

    // Positive counterpart: two exact 6-byte records verify cleanly.
    let ok_data: Vec<u8> = [0x02, 0x00, 0x00, 0x00, b'A', b'B']
        .into_iter()
        .chain([0x02, 0x00, 0x00, 0x00, b'C', b'D'])
        .collect();
    let mut exact = oversized.clone();
    assert!(
        exact.verify_coverage(&ok_data).is_ok(),
        "exact length-prefixed split must verify"
    );
    exact.ranges = vec![(0, 5), (5, 12)];
    assert_eq!(
        exact.verify_coverage(&ok_data).unwrap_err().kind,
        CoverageKind::Misaligned
    );

    // Canonical bytes are order-sensitive: reordered ranges are a
    // different manifest, never silently the same bytes.
    let mut reordered = plan.clone();
    reordered.ranges.reverse();
    assert_ne!(
        reordered.canonical_bytes(),
        plan.canonical_bytes(),
        "range order must be part of the canonical encoding"
    );

    fs::remove_file(path).unwrap();
}

#[test]
fn manifest_from_json_roundtrips_planner_output() {
    let data = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\n";
    let path = write_fixture("manifest_roundtrip", data);
    let plan = delimiter_manifest_plan(&path, 3);
    assert!(plan.ranges.len() >= 2);

    let text = plan.to_json();
    let restored = RangePlan::from_json(&text).expect("planner output must parse");
    assert_eq!(
        restored, plan,
        "roundtrip must preserve full plan semantics"
    );
    assert_eq!(
        restored.canonical_bytes(),
        plan.canonical_bytes(),
        "re-serializing the restored plan must reproduce the canonical bytes"
    );

    // Optional sections roundtrip too: index reference, source mode,
    // and an explicit window survive the manifest encoding.
    let identity = identify_file(&path).unwrap();
    let mut indexed = plan_from_ranges(
        &path,
        &identity,
        FramingDescriptor::Delimiter {
            delimiter: b"\n".to_vec(),
        },
        3,
        IndexReference {
            stride: 8,
            record_count: 6,
            path: Some("records.mmapidx".to_owned()),
        },
        plan.ranges.clone(),
    )
    .unwrap();
    indexed.source_mode = Some(SourceMode::Windowed);
    indexed.window_bytes = Some(65_536);
    let rebuilt = RangePlan::from_json(&indexed.to_json()).expect("indexed plan must parse");
    assert_eq!(rebuilt, indexed);
    assert_eq!(rebuilt.source_mode, Some(SourceMode::Windowed));
    assert_eq!(rebuilt.window_bytes, Some(65_536));
    assert!(rebuilt.index.is_some());

    fs::remove_file(path).unwrap();
}

#[test]
fn manifest_from_json_roundtrips_unicode_paths() {
    // Writer escapes control characters as \uXXXX while keeping
    // non-ASCII raw; the parser must accept both spellings equally.
    let data = b"eins\nzwei\ndrei\nvier\n";
    let directory =
        std::env::temp_dir().join(format!("mmap_chunker_core_unicode_{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("Grüße-✓-data.log");
    fs::write(&path, data).unwrap();
    let plan = delimiter_manifest_plan(&path, 2);
    let restored = RangePlan::from_json(&plan.to_json()).expect("unicode manifest must parse");
    assert_eq!(restored, plan);
    assert_eq!(restored.source_path, plan.source_path);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn manifest_from_json_rejects_malformed_documents() {
    let data = b"r01\nr02\nr03\nr04\n";
    let path = write_fixture("manifest_malformed", data);
    let plan = delimiter_manifest_plan(&path, 2);
    let valid = plan.to_json();

    let reject = |input: &str, kind: ManifestErrorKind| {
        assert_eq!(
            RangePlan::from_json(input).unwrap_err().kind,
            kind,
            "input: {input:?}"
        );
    };

    // Not JSON at all, or cut off mid-document.
    reject("", ManifestErrorKind::InvalidJson);
    reject("{", ManifestErrorKind::InvalidJson);
    let midpoint = valid[..valid.len() / 2]
        .rfind('\n')
        .unwrap_or(valid.len() / 2);
    reject(&valid[..midpoint], ManifestErrorKind::InvalidJson);
    reject("null", ManifestErrorKind::InvalidSchema);
    reject("[1, 2]", ManifestErrorKind::InvalidSchema);
    reject("{}", ManifestErrorKind::InvalidSchema);

    // Duplicate keys are ambiguous (first-wins lookup) and rejected.
    let duplicated = format!("{{\"schema\": \"x\",{}", &valid[1..]);
    reject(&duplicated, ManifestErrorKind::InvalidSchema);
    let duplicated_ranges = valid.replacen(
        "\"ranges\": [",
        "\"ranges\": [{\"index\": 0, \"start\": 0, \"end\": 0, \"length\": 0},",
        1,
    );
    // A prepended entry shifts every original index out of position.
    reject(&duplicated_ranges, ManifestErrorKind::InvalidSchema);

    // Schema identity and version.
    reject(
        &valid.replacen("mmap-chunker-plan", "mmap-chunker-other", 1),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &valid.replacen("\"schema_version\": 1", "\"schema_version\": 2", 1),
        ManifestErrorKind::UnsupportedVersion,
    );
    reject(
        &valid.replacen("\"schema_version\": 1", "\"schema_version\": \"1\"", 1),
        ManifestErrorKind::InvalidSchema,
    );

    // Numeric abuse: negatives, fractions, overflow.
    reject(
        &valid.replacen("\"size\": 16", "\"size\": -1", 1),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &valid.replacen("\"size\": 16", "\"size\": 1.5", 1),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &valid.replacen("\"size\": 16", "\"size\": 18446744073709551616", 1),
        ManifestErrorKind::InvalidSchema,
    );

    // Per-range structural violations fail at parse time.
    let entry = |index: usize, start: usize, end: usize, length: usize| {
        format!("{{\"index\": {index}, \"start\": {start}, \"end\": {end}, \"length\": {length}}}")
    };
    // NOTE: entries are spliced into the canonical tail; keep `]\n}`.
    let with_ranges_tail = |ranges: &str| {
        let cut = valid.find("\"ranges\": [").unwrap() + "\"ranges\": [".len();
        let tail = valid.find(']').unwrap();
        format!("{}{}{}", &valid[..cut], ranges, &valid[tail..])
    };
    reject(
        &with_ranges_tail(&entry(0, 0, 0, 0)),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &with_ranges_tail(&entry(0, 8, 4, 4)),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &with_ranges_tail(&entry(0, 0, 8, 7)),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &with_ranges_tail(&format!("{},{}", entry(1, 0, 8, 8), entry(0, 8, 16, 8))),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &with_ranges_tail(&entry(0, 0, 17, 17)),
        ManifestErrorKind::InvalidSchema,
    );
    // Entry must be an object; ranges must be an array.
    reject(&with_ranges_tail("5"), ManifestErrorKind::InvalidSchema);
    reject(
        &valid.replacen("\"ranges\": [", "\"ranges\": {", 1),
        ManifestErrorKind::InvalidJson,
    );

    // Framing abuse.
    reject(
        &valid.replacen("\"strategy\": \"delimiter\"", "\"strategy\": \"wat\"", 1),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &valid.replacen("\"delimiter_hex\": \"0a\"", "\"delimiter_hex\": \"\"", 1),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &valid.replacen(
            "\"delimiter_hex\": \"0a\"",
            "\"delimiter_hex\": \"0a\", \"delimiter_len\": 2",
            1,
        ),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &valid.replacen(
            "\"strategy\": \"delimiter\",\n    \"delimiter_hex\": \"0a\",\n    \"delimiter_len\": 1",
            "\"strategy\": \"fixed_width\",\n    \"record_bytes\": 0",
            1,
        ),
        ManifestErrorKind::InvalidSchema,
    );
    reject(
        &valid.replacen(
            "\"strategy\": \"delimiter\",\n    \"delimiter_hex\": \"0a\",\n    \"delimiter_len\": 1",
            "\"strategy\": \"length_prefixed\",\n    \"prefix_bytes\": 9,\n    \"little_endian\": true,\n    \"length_includes_prefix\": false",
            1,
        ),
        ManifestErrorKind::InvalidSchema,
    );

    // Sealed identity must agree with the recorded size.
    reject(
        &valid.replacen("\"size\": 16", "\"size\": 15", 1),
        ManifestErrorKind::InvalidSchema,
    );

    fs::remove_file(path).unwrap();
}

#[test]
fn manifest_from_json_enforces_resource_limits() {
    // Oversized input is rejected before the JSON parser allocates.
    let huge = " ".repeat(MAX_MANIFEST_BYTES + 1);
    assert_eq!(
        RangePlan::from_json(&huge).unwrap_err().kind,
        ManifestErrorKind::TooLarge
    );

    // The range cap fires before per-entry parsing: entries may be
    // trivially invalid, the count alone rejects the document.
    let data = b"a\n";
    let path = write_fixture("manifest_limits", data);
    let plan = delimiter_manifest_plan(&path, 1);
    let valid = plan.to_json();
    let cut = valid.find("\"ranges\": [").unwrap() + "\"ranges\": [".len();
    let mut big = valid[..cut].to_owned();
    big.push_str(&"0,".repeat(MAX_MANIFEST_RANGES + 1));
    big.push_str("0]}");
    assert_eq!(
        RangePlan::from_json(&big).unwrap_err().kind,
        ManifestErrorKind::TooManyRanges
    );
    fs::remove_file(path).unwrap();
}
