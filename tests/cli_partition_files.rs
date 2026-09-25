use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    worker: usize,
    source: usize,
    start: usize,
    end_exclusive: usize,
    length: usize,
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_mmap-chunker")
}

fn write_sources(label: &str, contents: &[&[u8]]) -> (PathBuf, Vec<PathBuf>) {
    let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "mmap_chunker_cli_files_{label}_{}_{}",
        std::process::id(),
        sequence
    ));
    fs::create_dir_all(&directory).unwrap();
    let paths = contents
        .iter()
        .enumerate()
        .map(|(index, content)| {
            let path = directory.join(format!("source-{index}.dat"));
            fs::write(&path, content).unwrap();
            path
        })
        .collect();
    (directory, paths)
}

fn run_partition_files(paths: &[&Path], parts: usize, delimiter: Option<u8>) -> Output {
    let mut arguments = vec![
        OsString::from("partition-files"),
        OsString::from("--parts"),
        OsString::from(parts.to_string()),
    ];
    if let Some(delimiter) = delimiter {
        arguments.push(OsString::from("--delimiter-byte"));
        arguments.push(OsString::from(delimiter.to_string()));
    }
    arguments.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    Command::new(binary()).args(arguments).output().unwrap()
}

fn run_partition_files_hex(paths: &[&Path], parts: usize, hex: &str) -> Output {
    let mut arguments = vec![
        OsString::from("partition-files"),
        OsString::from("--parts"),
        OsString::from(parts.to_string()),
        OsString::from("--delimiter-hex"),
        OsString::from(hex),
    ];
    arguments.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    Command::new(binary()).args(arguments).output().unwrap()
}

fn parse_rows(stdout: &[u8]) -> Vec<Row> {
    let text = std::str::from_utf8(stdout).unwrap();
    text.lines()
        .enumerate()
        .map(|(line_number, line)| {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(
                fields.len(),
                5,
                "unexpected output line {line_number}: {line}"
            );
            Row {
                worker: fields[0].parse().unwrap(),
                source: fields[1].parse().unwrap(),
                start: fields[2].parse().unwrap(),
                end_exclusive: fields[3].parse().unwrap(),
                length: fields[4].parse().unwrap(),
            }
        })
        .collect()
}

fn is_record_boundary(data: &[u8], offset: usize, delimiter: u8) -> bool {
    offset == 0 || offset == data.len() || (offset > 0 && data[offset - 1] == delimiter)
}

fn is_record_boundary_pattern(data: &[u8], offset: usize, delimiter: &[u8]) -> bool {
    assert!(!delimiter.is_empty());
    offset == 0
        || offset == data.len()
        || (offset >= delimiter.len() && data[offset - delimiter.len()..offset] == delimiter[..])
}

fn assert_dataset_oracle(paths: &[PathBuf], parts: usize, delimiter: Option<u8>) -> Vec<Row> {
    let path_refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let first = run_partition_files(&path_refs, parts, delimiter);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());

    let second = run_partition_files(&path_refs, parts, delimiter);
    assert!(second.status.success());
    assert_eq!(
        first.stdout, second.stdout,
        "CLI output was not deterministic"
    );

    let delimiter = delimiter.unwrap_or(b'\n');
    let source_bytes: Vec<Vec<u8>> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
    let expected: Vec<u8> = source_bytes.iter().flatten().copied().collect();
    let rows = parse_rows(&first.stdout);
    if expected.is_empty() {
        assert!(rows.is_empty(), "all-empty datasets must emit empty stdout");
        return rows;
    }
    assert!(!rows.is_empty());
    assert_eq!(rows[0].worker, 0);

    let mut cursors = vec![0usize; source_bytes.len()];
    let mut worker_bytes = Vec::new();
    let mut reconstructed = Vec::new();
    let mut previous_worker = 0;
    let mut previous_source = 0;
    for (row_index, row) in rows.iter().copied().enumerate() {
        if row_index > 0 {
            assert!(row.worker == previous_worker || row.worker == previous_worker + 1);
            if row.worker == previous_worker {
                assert!(row.source >= previous_source);
            }
        }
        assert!(row.source < source_bytes.len());
        let source = &source_bytes[row.source];
        assert!(row.start <= row.end_exclusive);
        assert!(row.end_exclusive <= source.len());
        assert_eq!(row.end_exclusive - row.start, row.length);
        assert!(is_record_boundary(source, row.start, delimiter));
        assert!(is_record_boundary(source, row.end_exclusive, delimiter));
        assert_eq!(
            row.start, cursors[row.source],
            "gap or overlap in source range"
        );
        cursors[row.source] = row.end_exclusive;
        reconstructed.extend_from_slice(&source[row.start..row.end_exclusive]);

        while worker_bytes.len() <= row.worker {
            worker_bytes.push(0);
        }
        worker_bytes[row.worker] += row.length;
        previous_worker = row.worker;
        previous_source = row.source;
    }

    for (source_index, (cursor, source)) in cursors.iter().zip(&source_bytes).enumerate() {
        assert_eq!(
            *cursor,
            source.len(),
            "source {source_index} was not covered exactly"
        );
    }
    assert_eq!(
        reconstructed, expected,
        "worker/source rows did not reconstruct dataset"
    );

    let ideal = expected.len() as f64 / worker_bytes.len() as f64;
    let max_worker = *worker_bytes.iter().max().unwrap();
    eprintln!(
        "partition-files balance: sources={} workers={} logical_bytes={} max_worker_bytes={} max_over_ideal={:.3}",
        paths.len(),
        worker_bytes.len(),
        expected.len(),
        max_worker,
        max_worker as f64 / ideal
    );
    rows
}

fn assert_dataset_oracle_pattern(
    paths: &[PathBuf],
    parts: usize,
    hex: &str,
    delimiter: &[u8],
) -> Vec<Row> {
    assert!(!delimiter.is_empty());
    let path_refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let first = run_partition_files_hex(&path_refs, parts, hex);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());

    let second = run_partition_files_hex(&path_refs, parts, hex);
    assert!(second.status.success());
    assert_eq!(
        first.stdout, second.stdout,
        "CLI output was not deterministic"
    );

    let source_bytes: Vec<Vec<u8>> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
    let expected: Vec<u8> = source_bytes.iter().flatten().copied().collect();
    let rows = parse_rows(&first.stdout);
    if expected.is_empty() {
        assert!(rows.is_empty(), "all-empty datasets must emit empty stdout");
        return rows;
    }
    assert!(!rows.is_empty());
    assert_eq!(rows[0].worker, 0);

    let mut cursors = vec![0usize; source_bytes.len()];
    let mut reconstructed = Vec::new();
    let mut previous_worker = 0;
    let mut previous_source = 0;
    for (row_index, row) in rows.iter().copied().enumerate() {
        if row_index > 0 {
            assert!(row.worker == previous_worker || row.worker == previous_worker + 1);
            if row.worker == previous_worker {
                assert!(row.source >= previous_source);
            }
        }
        assert!(row.source < source_bytes.len());
        let source = &source_bytes[row.source];
        assert!(row.start <= row.end_exclusive);
        assert!(row.end_exclusive <= source.len());
        assert_eq!(row.end_exclusive - row.start, row.length);
        assert!(
            is_record_boundary_pattern(source, row.start, delimiter),
            "row start split a record: {row:?}"
        );
        assert!(
            is_record_boundary_pattern(source, row.end_exclusive, delimiter),
            "row end split a record: {row:?}"
        );
        assert_eq!(
            row.start, cursors[row.source],
            "gap or overlap in source range"
        );
        cursors[row.source] = row.end_exclusive;
        reconstructed.extend_from_slice(&source[row.start..row.end_exclusive]);
        previous_worker = row.worker;
        previous_source = row.source;
    }

    for (source_index, (cursor, source)) in cursors.iter().zip(&source_bytes).enumerate() {
        assert_eq!(
            *cursor,
            source.len(),
            "source {source_index} was not covered exactly"
        );
    }
    assert_eq!(
        reconstructed, expected,
        "worker/source rows did not reconstruct dataset"
    );
    rows
}

#[test]
fn reconstructs_ordered_jsonl_sources_and_allows_scatter_gather_rows() {
    let (directory, paths) = write_sources(
        "jsonl_scatter",
        &[
            b"{\"id\":1}\n",
            b"",
            b"{\"id\":2}\n{\"id\":3}\n{\"id\":4,\"p\":\"x\"}\n",
            b"{\"id\":5}\n",
        ],
    );
    let rows = assert_dataset_oracle(&paths, 3, None);

    let mut rows_per_worker = vec![0usize; rows.last().unwrap().worker + 1];
    for row in rows {
        rows_per_worker[row.worker] += 1;
    }
    assert!(
        rows_per_worker.iter().any(|count| *count > 1),
        "at least one worker should receive multiple source ranges or records"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn keeps_file_boundary_as_a_logical_partition_boundary() {
    let (directory, paths) = write_sources("file_boundary", &[b"a\n", b"b\n"]);
    let rows = assert_dataset_oracle(&paths, 2, None);
    assert_eq!(
        rows,
        vec![
            Row {
                worker: 0,
                source: 0,
                start: 0,
                end_exclusive: 2,
                length: 2,
            },
            Row {
                worker: 1,
                source: 1,
                start: 0,
                end_exclusive: 2,
                length: 2,
            },
        ]
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn proves_empty_sources_no_final_delimiter_and_partition_count_edges() {
    let (directory, paths) = write_sources("all_empty", &[b"", b"", b""]);
    assert_dataset_oracle(&paths, 8, None);
    fs::remove_dir_all(directory).unwrap();

    let (directory, paths) = write_sources(
        "empty_edges",
        &[b"", b"", b"one\n", b"two\nthree", b"single record"],
    );
    assert_dataset_oracle(&paths, 1, None);
    let rows = assert_dataset_oracle(&paths, usize::MAX, None);
    assert!(
        rows.len() <= 5,
        "record alignment should collapse excess targets"
    );
    fs::remove_dir_all(directory).unwrap();

    let (directory, paths) = write_sources("giant_record", &[&[b'x'; 128], b"small\n"]);
    let rows = assert_dataset_oracle(&paths, 16, None);
    let worker_count = rows.last().unwrap().worker + 1;
    assert!(
        worker_count < 16,
        "a giant record must collapse worker targets"
    );
    fs::remove_dir_all(directory).unwrap();

    let huge_record = vec![b'x'; 2048];
    let (directory, paths) = write_sources("tiny_huge_tiny", &[b"a\n", &huge_record, b"z\n"]);
    assert_dataset_oracle(&paths, 8, None);
    fs::remove_dir_all(directory).unwrap();

    let (directory, paths) = write_sources("one_record", &[b"only record\n"]);
    let rows = assert_dataset_oracle(&paths, 8, None);
    assert_eq!(rows.len(), 1);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn supports_binary_delimiters_and_duplicate_paths_as_distinct_sources() {
    let (directory, paths) =
        write_sources("binary", &[&[0x01, 0x00, 0xFE, 0x00, 0x02, 0x00, 0x03]]);
    assert_dataset_oracle(&paths, 4, Some(0));

    let duplicate_paths = vec![paths[0].clone(), paths[0].clone()];
    let rows = assert_dataset_oracle(&duplicate_paths, 3, Some(0));
    assert!(rows.iter().any(|row| row.source == 0));
    assert!(rows.iter().any(|row| row.source == 1));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rejects_empty_file_list_missing_paths_and_invalid_options() {
    let no_files = run_partition_files(&[], 2, None);
    assert!(!no_files.status.success());
    assert!(no_files.stdout.is_empty());
    assert!(!no_files.stderr.is_empty());

    let missing = PathBuf::from("definitely-not-a-real-source-file.jsonl");
    let missing_output = run_partition_files(&[&missing], 2, None);
    assert!(!missing_output.status.success());
    assert!(missing_output.stdout.is_empty());
    assert!(!missing_output.stderr.is_empty());

    let (directory, paths) = write_sources("invalid", &[b"a\n"]);
    let invalid_parts = Command::new(binary())
        .args([
            OsString::from("partition-files"),
            OsString::from("--parts"),
            OsString::from("0"),
            paths[0].as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!invalid_parts.status.success());
    assert!(invalid_parts.stdout.is_empty());

    let invalid_delimiter = Command::new(binary())
        .args([
            OsString::from("partition-files"),
            OsString::from("--parts"),
            OsString::from("1"),
            OsString::from("--delimiter-byte"),
            OsString::from("256"),
            paths[0].as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!invalid_delimiter.status.success());
    assert!(invalid_delimiter.stdout.is_empty());
    let unexpected_worker = Command::new(binary())
        .args([
            OsString::from("partition-files"),
            OsString::from("--parts"),
            OsString::from("1"),
            OsString::from("--worker"),
            OsString::from("0"),
            paths[0].as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!unexpected_worker.status.success());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn opens_all_sources_before_emitting_machine_readable_output() {
    let (directory, paths) = write_sources("atomic_open", &[b"first\n", b"second\n"]);
    let missing = directory.join("missing-middle.dat");
    let output = run_partition_files(&[&paths[0], &paths[1], &missing], 3, None);

    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "open failures must not emit TSV rows"
    );
    assert!(!output.stderr.is_empty());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn help_documents_the_multi_file_contract() {
    let output = Command::new(binary())
        .args([OsString::from("partition-files"), OsString::from("--help")])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("partition-files --parts N"));
    assert!(help.contains("worker<TAB>source<TAB>start<TAB>end_exclusive<TAB>length"));
    assert!(help.contains("ordered logical dataset"));
    assert!(help.contains("all-empty dataset succeeds"));
}

#[test]
fn hex_crlf_partitions_records_across_sources() {
    let (directory, paths) = write_sources(
        "hex_crlf",
        &[
            b"row-1\r\nrow-2\r\n",
            b"",
            b"row-3\r\nrow-4\r\nrow-5\r\n",
            b"row-6\r\n",
        ],
    );
    let rows = assert_dataset_oracle_pattern(&paths, 3, "0d0a", b"\r\n");
    assert!(rows.iter().any(|row| row.source == 0));
    assert!(rows.iter().any(|row| row.source == 2));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn hex_single_byte_matches_delimiter_byte_exactly() {
    let (directory, paths) =
        write_sources("hex_parity", &[b"one\ntwo\nthree\nfour\n", b"five\nsix\n"]);
    let path_refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    for parts in [1, 2, 3, 8] {
        let byte = run_partition_files(&path_refs, parts, Some(b'\n'));
        let hex = run_partition_files_hex(&path_refs, parts, "0a");
        let upper_hex = run_partition_files_hex(&path_refs, parts, "0A");
        assert!(byte.status.success() && hex.status.success() && upper_hex.status.success());
        assert_eq!(
            hex.stdout, byte.stdout,
            "hex 0a diverged from --delimiter-byte 10 at parts={parts}"
        );
        assert_eq!(
            upper_hex.stdout, byte.stdout,
            "hex 0A diverged from --delimiter-byte 10 at parts={parts}"
        );
    }
    // NUL through hex takes the same single-byte path as --delimiter-byte 0.
    let (nul_directory, nul_paths) =
        write_sources("hex_nul_parity", &[&[0x01, 0x00, 0x02, 0x00, 0x03]]);
    let nul_refs: Vec<_> = nul_paths.iter().map(PathBuf::as_path).collect();
    let byte = run_partition_files(&nul_refs, 2, Some(0));
    let hex = run_partition_files_hex(&nul_refs, 2, "00");
    assert!(byte.status.success() && hex.status.success());
    assert_eq!(hex.stdout, byte.stdout);
    fs::remove_dir_all(directory).unwrap();
    fs::remove_dir_all(nul_directory).unwrap();
}

#[test]
fn hex_handles_overlapping_and_repeated_patterns() {
    // Overlapping pattern "AA": occurrences at 1, 4, 7 in "aAAbAAcAA".
    // Targets 3 and 6 already follow a complete pattern, so both are
    // accepted exactly (mirrors the single-file exact-target rule).
    let (directory, paths) = write_sources("hex_overlap", &[b"aAAbAAcAA"]);
    let rows = assert_dataset_oracle_pattern(&paths, 3, "4141", b"AA");
    assert_eq!(
        rows,
        vec![
            Row {
                worker: 0,
                source: 0,
                start: 0,
                end_exclusive: 3,
                length: 3,
            },
            Row {
                worker: 1,
                source: 0,
                start: 3,
                end_exclusive: 6,
                length: 3,
            },
            Row {
                worker: 2,
                source: 0,
                start: 6,
                end_exclusive: 9,
                length: 3,
            },
        ]
    );
    fs::remove_dir_all(directory).unwrap();

    // Consecutive CRLF records and a missing trailing delimiter.
    let (directory, paths) =
        write_sources("hex_consecutive", &[b"\r\n\r\n\r\n", b"tail-no-delimiter"]);
    assert_dataset_oracle_pattern(&paths, 4, "0d0a", b"\r\n");
    fs::remove_dir_all(directory).unwrap();

    // Double-CRLF framing and a delimiter longer than a source.
    let (directory, paths) = write_sources(
        "hex_double_crlf",
        &[b"a\r\n\r\nb\r\n\r\n", b"\r\n", b"tiny"],
    );
    assert_dataset_oracle_pattern(&paths, 4, "0d0a0d0a", b"\r\n\r\n");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn hex_handles_empty_giant_and_collapsed_targets() {
    let (directory, paths) = write_sources("hex_all_empty", &[b"", b"", b""]);
    assert_dataset_oracle_pattern(&paths, 8, "0d0a", b"\r\n");
    fs::remove_dir_all(directory).unwrap();

    let (directory, paths) = write_sources(
        "hex_empty_edges",
        &[b"", b"one\r\n", b"two\r\nthree", b"single record"],
    );
    assert_dataset_oracle_pattern(&paths, 1, "0d0a", b"\r\n");
    let rows = assert_dataset_oracle_pattern(&paths, usize::MAX, "0d0a", b"\r\n");
    assert!(
        rows.len() <= 5,
        "record alignment should collapse excess targets"
    );
    fs::remove_dir_all(directory).unwrap();

    let giant_record = vec![b'x'; 2048];
    let (directory, paths) = write_sources("hex_giant", &[b"a\r\n", &giant_record, b"z\r\n"]);
    let rows = assert_dataset_oracle_pattern(&paths, 16, "0d0a", b"\r\n");
    let worker_count = rows.last().unwrap().worker + 1;
    assert!(
        worker_count < 16,
        "a giant record must collapse worker targets"
    );
    fs::remove_dir_all(directory).unwrap();

    // Duplicate paths stay distinct sources under a hex delimiter.
    let (directory, paths) = write_sources("hex_duplicate", &[b"k\r\nv\r\n"]);
    let duplicates = vec![paths[0].clone(), paths[0].clone()];
    let rows = assert_dataset_oracle_pattern(&duplicates, 3, "0d0a", b"\r\n");
    assert!(rows.iter().any(|row| row.source == 0));
    assert!(rows.iter().any(|row| row.source == 1));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn hex_rejects_invalid_forms_and_conflicts() {
    let (directory, paths) = write_sources("hex_invalid", &[b"a\r\nb\r\n"]);
    let path_refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    for hex in ["", "0", "0d0", "0x0a", "zz", "0d 0a", "-1"] {
        let output = run_partition_files_hex(&path_refs, 2, hex);
        assert!(
            !output.status.success(),
            "hex `{hex}` unexpectedly succeeded"
        );
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("--delimiter-hex"),
            "stderr does not name the option: {stderr}"
        );
    }

    let both = Command::new(binary())
        .args([
            OsString::from("partition-files"),
            OsString::from("--parts"),
            OsString::from("2"),
            OsString::from("--delimiter-byte"),
            OsString::from("10"),
            OsString::from("--delimiter-hex"),
            OsString::from("0d0a"),
            paths[0].as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!both.status.success());
    assert!(both.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("duplicate delimiter option"),
        "stderr: {}",
        String::from_utf8_lossy(&both.stderr)
    );

    let duplicate_hex = Command::new(binary())
        .args([
            OsString::from("partition-files"),
            OsString::from("--parts"),
            OsString::from("2"),
            OsString::from("--delimiter-hex"),
            OsString::from("0d0a"),
            OsString::from("--delimiter-hex"),
            OsString::from("0a"),
            paths[0].as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!duplicate_hex.status.success());
    assert!(duplicate_hex.stdout.is_empty());
    fs::remove_dir_all(directory).unwrap();
}
