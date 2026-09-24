use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
type BinaryDelimiterCase<'a> = (&'a str, &'a [u8], u8, &'a str, Option<usize>);
type PatternPartitionCase<'a> = (&'a str, &'a [u8], &'a str, &'a [u8], Option<usize>);

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_mmap-chunker")
}

fn fixture_dir(label: &str) -> PathBuf {
    let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mmap_chunker_cli_{label}_{}_{}",
        std::process::id(),
        sequence
    ))
}

fn write_fixture(label: &str, name: OsString, contents: &[u8]) -> (PathBuf, PathBuf) {
    let directory = fixture_dir(label);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    fs::write(&path, contents).unwrap();
    (directory, path)
}

fn run(arguments: &[&std::ffi::OsStr]) -> Output {
    Command::new(binary()).args(arguments).output().unwrap()
}

fn run_os(arguments: &[OsString]) -> Output {
    Command::new(binary()).args(arguments).output().unwrap()
}

fn run_partition(path: &Path, parts: &str, delimiter: Option<u8>, worker: Option<usize>) -> Output {
    let mut arguments = vec![
        OsString::from("partition"),
        path.as_os_str().to_owned(),
        OsString::from("--parts"),
        OsString::from(parts),
    ];
    if let Some(delimiter) = delimiter {
        arguments.push(OsString::from("--delimiter-byte"));
        arguments.push(OsString::from(delimiter.to_string()));
    }
    if let Some(worker) = worker {
        arguments.push(OsString::from("--worker"));
        arguments.push(OsString::from(worker.to_string()));
    }
    Command::new(binary()).args(arguments).output().unwrap()
}

fn run_partition_hex(path: &Path, parts: &str, hex: &str, worker: Option<usize>) -> Output {
    let mut arguments = vec![
        OsString::from("partition"),
        path.as_os_str().to_owned(),
        OsString::from("--parts"),
        OsString::from(parts),
        OsString::from("--delimiter-hex"),
        OsString::from(hex),
    ];
    if let Some(worker) = worker {
        arguments.push(OsString::from("--worker"));
        arguments.push(OsString::from(worker.to_string()));
    }
    Command::new(binary()).args(arguments).output().unwrap()
}

fn parse_ranges(stdout: &[u8]) -> Vec<(usize, usize, usize, usize)> {
    let text = std::str::from_utf8(stdout).unwrap();
    text.lines()
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(fields.len(), 4, "unexpected output line: {line}");
            (
                fields[0].parse().unwrap(),
                fields[1].parse().unwrap(),
                fields[2].parse().unwrap(),
                fields[3].parse().unwrap(),
            )
        })
        .collect()
}

fn assert_worker_projection_oracle(path: &Path, parts: usize, delimiter: Option<u8>) {
    let parts_text = parts.to_string();
    let full = run_partition(path, &parts_text, delimiter, None);
    assert!(
        full.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert!(full.stderr.is_empty());
    let ranges = parse_ranges(&full.stdout);

    for worker in 0..parts {
        let selected = run_partition(path, &parts_text, delimiter, Some(worker));
        assert!(
            selected.status.success(),
            "worker {worker} stderr: {}",
            String::from_utf8_lossy(&selected.stderr)
        );
        assert!(selected.stderr.is_empty(), "worker {worker} wrote stderr");

        let expected = ranges
            .get(worker)
            .map(|(index, start, end, length)| format!("{index}\t{start}\t{end}\t{length}\n"))
            .unwrap_or_default();
        assert_eq!(
            String::from_utf8(selected.stdout).unwrap(),
            expected,
            "worker {worker} was not the exact projection of the full plan"
        );
    }
}

fn assert_partition_oracle(path: &Path, parts: &str, expected_count: Option<usize>) {
    assert_partition_oracle_with_delimiter(path, parts, None, b'\n', expected_count);
}

fn assert_partition_oracle_with_delimiter(
    path: &Path,
    parts: &str,
    delimiter: Option<u8>,
    expected_delimiter: u8,
    expected_count: Option<usize>,
) {
    let first = run_partition(path, parts, delimiter, None);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());
    let second = run_partition(path, parts, delimiter, None);
    assert_eq!(
        first.stdout, second.stdout,
        "CLI output was not deterministic"
    );

    let source = fs::read(path).unwrap();
    let ranges = parse_ranges(&first.stdout);
    if let Some(expected_count) = expected_count {
        assert_eq!(ranges.len(), expected_count);
    }
    if source.is_empty() {
        assert!(ranges.is_empty());
        return;
    }

    let mut cursor = 0;
    let mut reconstructed = Vec::new();
    for (expected_index, (index, start, end, length)) in ranges.iter().copied().enumerate() {
        assert_eq!(index, expected_index);
        assert_eq!(start, cursor, "gap or overlap at range {index}");
        assert_eq!(end - start, length);
        assert!(end <= source.len());
        reconstructed.extend_from_slice(&source[start..end]);
        if index + 1 < ranges.len() {
            assert_eq!(
                source[end - 1],
                expected_delimiter,
                "range {index} split a record"
            );
        }
        cursor = end;
    }
    assert_eq!(
        cursor,
        source.len(),
        "ranges did not cover the complete file"
    );
    assert_eq!(
        reconstructed, source,
        "ranges did not reconstruct the source"
    );
}

#[test]
fn partitions_cover_representative_record_layouts() {
    let cases: &[(&str, &[u8], &str, Option<usize>)] = &[
        ("empty", b"", "8", Some(0)),
        ("one_partition", b"a\nb\nc\n", "1", Some(1)),
        ("one_record", b"only record", "8", Some(1)),
        ("final_newline", b"a\nb\nc\n", "2", Some(2)),
        ("no_final_newline", b"a\nb\nc", "4", None),
        (
            "uneven_jsonl",
            b"{\"id\":1}\n{\"id\":2,\"payload\":\"longer\"}\n{\"id\":3}\n",
            "4",
            None,
        ),
        (
            "giant_record",
            b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\nsmall\n",
            "8",
            Some(2),
        ),
        ("sparse", b"aaaaaaaaaaaaaaaaaaaa\nb\n", "8", Some(2)),
    ];
    for (label, contents, parts, expected_count) in cases {
        let (directory, path) = write_fixture(label, OsString::from("records.jsonl"), contents);
        assert_partition_oracle(&path, parts, *expected_count);
        assert_worker_projection_oracle(&path, parts.parse().unwrap(), None);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn configurable_delimiter_covers_required_bytes_and_default_equivalence() {
    let cases: &[(&str, &[u8], u8, &str)] = &[
        ("lf", b"one\ntwo\nthree\n", b'\n', "records.txt"),
        (
            "nul",
            &[0x01, 0x00, 0xFE, 0x02, 0x00, 0x00, 0x80, 0x00],
            0x00,
            "records.bin",
        ),
        ("comma", b"left,right,,tail", b',', "records.bin"),
        ("pipe", b"left|middle||tail", b'|', "records.bin"),
        (
            "ff",
            &[0x00, 0xFF, 0x10, 0xFF, 0xFF, 0x80],
            0xFF,
            "records.bin",
        ),
    ];

    for (label, contents, delimiter, name) in cases {
        let (directory, path) = write_fixture(label, OsString::from(name), contents);
        assert_partition_oracle_with_delimiter(&path, "8", Some(*delimiter), *delimiter, None);
        assert_worker_projection_oracle(&path, 8, Some(*delimiter));

        if *delimiter == b'\n' {
            let default = run_partition(&path, "8", None, None);
            let explicit = run_partition(&path, "8", Some(0x0A), None);
            assert!(default.status.success());
            assert!(explicit.status.success());
            assert_eq!(default.stdout, explicit.stdout);
            assert_eq!(default.stderr, explicit.stderr);
        }
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn configurable_delimiter_covers_binary_and_boundary_edge_cases() {
    let cases: &[BinaryDelimiterCase<'_>] = &[
        ("empty_binary", b"", 0xFF, "8", Some(0)),
        ("no_delimiter", &[0x01, 0x02, 0x03], 0x00, "4", Some(1)),
        ("every_byte_delimiter", &[0, 0, 0, 0], 0x00, "8", None),
        (
            "delimiter_at_start_and_eof",
            &[0, 0x10, 0x20, 0],
            0x00,
            "2",
            None,
        ),
        ("no_final_delimiter", b"aa\0bb\0cc", 0x00, "4", None),
        (
            "giant_record",
            &[0x10, 0x10, 0x10, 0x10, 0x10, 0x7C, 0x01, 0x7C],
            0x7C,
            "8",
            None,
        ),
        (
            "sparse_delimiters",
            &[0x20, 0x20, 0x20, 0x20, 0x2C, 0x01, 0x2C],
            b',',
            "8",
            None,
        ),
    ];

    for (label, contents, delimiter, parts, expected_count) in cases {
        let (directory, path) =
            write_fixture(label, OsString::from("binary records.bin"), contents);
        assert_partition_oracle_with_delimiter(
            &path,
            parts,
            Some(*delimiter),
            *delimiter,
            *expected_count,
        );
        assert_worker_projection_oracle(&path, parts.parse().unwrap(), Some(*delimiter));
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn cr_delimiter_is_single_byte_not_crlf() {
    let (directory, path) = write_fixture(
        "crlf_semantics",
        OsString::from("records-ä.txt"),
        b"a\r\nb\r\nc\r\n",
    );
    let output = run_partition(&path, "2", Some(0x0D), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ranges = parse_ranges(&output.stdout);
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0].2, 5);
    let source = fs::read(&path).unwrap();
    assert_eq!(source[ranges[0].2 - 1], 0x0D);
    assert_eq!(source[ranges[0].2], 0x0A);
    fs::remove_dir_all(directory).unwrap();
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn assert_pattern_partition_oracle(
    path: &Path,
    parts: &str,
    pattern: &[u8],
    expected_count: Option<usize>,
) {
    let hex = hex_encode(pattern);
    let first = run_partition_hex(path, parts, &hex, None);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());
    let second = run_partition_hex(path, parts, &hex, None);
    assert_eq!(
        first.stdout, second.stdout,
        "CLI output was not deterministic"
    );

    let source = fs::read(path).unwrap();
    let ranges = parse_ranges(&first.stdout);
    if let Some(expected_count) = expected_count {
        assert_eq!(ranges.len(), expected_count);
    }
    if source.is_empty() {
        assert!(ranges.is_empty());
        return;
    }

    let mut cursor = 0;
    let mut reconstructed = Vec::new();
    for (expected_index, (index, start, end, length)) in ranges.iter().copied().enumerate() {
        assert_eq!(index, expected_index);
        assert_eq!(start, cursor, "gap or overlap at range {index}");
        assert_eq!(end - start, length);
        assert!(end <= source.len());
        reconstructed.extend_from_slice(&source[start..end]);
        if index + 1 < ranges.len() {
            assert!(
                end >= pattern.len(),
                "range {index} is shorter than the delimiter pattern"
            );
            assert_eq!(
                &source[end - pattern.len()..end],
                pattern,
                "range {index} did not end on the complete delimiter pattern"
            );
        }
        cursor = end;
    }
    assert_eq!(
        cursor,
        source.len(),
        "ranges did not cover the complete file"
    );
    assert_eq!(
        reconstructed, source,
        "ranges did not reconstruct the source"
    );

    let parts_value: usize = parts.parse().unwrap();
    for worker in 0..parts_value {
        let selected = run_partition_hex(path, parts, &hex, Some(worker));
        assert!(
            selected.status.success(),
            "worker {worker} stderr: {}",
            String::from_utf8_lossy(&selected.stderr)
        );
        assert!(selected.stderr.is_empty(), "worker {worker} wrote stderr");
        let expected = ranges
            .get(worker)
            .map(|(index, start, end, length)| format!("{index}\t{start}\t{end}\t{length}\n"))
            .unwrap_or_default();
        assert_eq!(
            String::from_utf8(selected.stdout).unwrap(),
            expected,
            "worker {worker} was not the exact projection of the full plan"
        );
    }
}

#[test]
fn multi_byte_hex_delimiter_covers_record_framings() {
    let cases: &[PatternPartitionCase<'_>] = &[
        ("crlf", b"a\r\nb\r\nc\r\n", "2", b"\r\n", None),
        ("crlf_no_final", b"a\r\nb\r\nc", "4", b"\r\n", None),
        (
            "blank_line",
            b"head1\r\n\r\nhead2\r\n\r\nhead3\r\n\r\n",
            "2",
            b"\r\n\r\n",
            None,
        ),
        (
            "binary_pair",
            &[0x10, 0x00, 0x01, 0x20, 0x00, 0x01, 0x30],
            "2",
            &[0x00, 0x01],
            None,
        ),
        ("no_pattern", b"nothing to see here", "4", b"\r\n", Some(1)),
        ("empty", b"", "8", b"\r\n", Some(0)),
        ("one_partition", b"a\r\nb\r\n", "1", b"\r\n", Some(1)),
        (
            "giant_record",
            b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\nsmall\r\n",
            "8",
            b"\r\n",
            None,
        ),
    ];

    for (label, contents, parts, pattern, expected_count) in cases {
        let (directory, path) = write_fixture(label, OsString::from("records.bin"), contents);
        assert_pattern_partition_oracle(&path, parts, pattern, *expected_count);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn hex_delimiter_matches_byte_delimiter_for_single_byte_patterns() {
    let (directory, path) = write_fixture(
        "hex_byte_equivalence",
        OsString::from("records.txt"),
        b"one\ntwo\nthree\nfour\n",
    );
    let default = run_partition(&path, "3", None, None);
    let byte = run_partition(&path, "3", Some(0x0A), None);
    let hex = run_partition_hex(&path, "3", "0a", None);
    assert!(default.status.success());
    assert!(byte.status.success());
    assert!(hex.status.success());
    assert_eq!(default.stdout, byte.stdout);
    assert_eq!(byte.stdout, hex.stdout);
    fs::remove_dir_all(directory).unwrap();
}

fn run_partition_source(
    path: &Path,
    parts: &str,
    delimiter_hex: Option<&str>,
    source: &str,
    window: Option<&str>,
    worker: Option<usize>,
) -> Output {
    let mut arguments = vec![
        OsString::from("partition"),
        path.as_os_str().to_owned(),
        OsString::from("--parts"),
        OsString::from(parts),
        OsString::from("--source"),
        OsString::from(source),
    ];
    if let Some(hex) = delimiter_hex {
        arguments.push(OsString::from("--delimiter-hex"));
        arguments.push(OsString::from(hex));
    }
    if let Some(window) = window {
        arguments.push(OsString::from("--window"));
        arguments.push(OsString::from(window));
    }
    if let Some(worker) = worker {
        arguments.push(OsString::from("--worker"));
        arguments.push(OsString::from(worker.to_string()));
    }
    Command::new(binary()).args(arguments).output().unwrap()
}

#[test]
fn source_backends_produce_identical_ranges() {
    let mut content = Vec::new();
    for index in 0..5_000u32 {
        content.extend_from_slice(format!("record-{index:05},payload-{index:05}\r\n").as_bytes());
    }
    let (directory, path) = write_fixture("source_modes", OsString::from("records.log"), &content);

    let baseline = run_partition_hex(&path, "16", "0d0a", None);
    assert!(
        baseline.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );

    for source in ["mmap", "windowed", "pread"] {
        let window = if source == "windowed" {
            Some("65536")
        } else {
            None
        };
        let output = run_partition_source(&path, "16", Some("0d0a"), source, window, None);
        assert!(
            output.status.success(),
            "source {source} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert_eq!(
            output.stdout, baseline.stdout,
            "source {source} diverged from mmap baseline"
        );

        for worker in [0usize, 3, 15] {
            let selected =
                run_partition_source(&path, "16", Some("0d0a"), source, window, Some(worker));
            assert!(selected.status.success());
            let ranges = parse_ranges(&baseline.stdout);
            let expected = ranges
                .get(worker)
                .map(|(index, start, end, length)| format!("{index}\t{start}\t{end}\t{length}\n"))
                .unwrap_or_default();
            assert_eq!(
                String::from_utf8(selected.stdout).unwrap(),
                expected,
                "source {source} worker {worker} projection mismatch"
            );
        }
    }

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn fixed_width_framing_partitions_on_record_boundaries() {
    let content: Vec<u8> = (0..100u8).collect();
    let (directory, path) = write_fixture("framing_fixed", OsString::from("records.bin"), &content);

    let base = vec![
        OsString::from("partition"),
        path.as_os_str().to_owned(),
        OsString::from("--parts"),
        OsString::from("4"),
        OsString::from("--framing"),
        OsString::from("fixed"),
        OsString::from("--record-bytes"),
        OsString::from("16"),
    ];
    let output = run_os(&base);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        parse_ranges(&output.stdout),
        vec![
            (0, 0, 32, 32),
            (1, 32, 64, 32),
            (2, 64, 80, 16),
            (3, 80, 100, 20)
        ]
    );

    let mut selected = base.clone();
    selected.push(OsString::from("--worker"));
    selected.push(OsString::from("2"));
    let output = run_os(&selected);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "2\t64\t80\t16\n");

    // Every non-final boundary is a record edge.
    for (start, end) in [(0usize, 32usize), (32, 64), (64, 80)] {
        assert_eq!(start % 16, 0);
        assert_eq!(end % 16, 0);
    }

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn length_prefixed_framing_partitions_on_records() {
    let payloads: &[&[u8]] = &[b"alpha", b"be", b"gamma-payload", b""];
    let mut content = Vec::new();
    for payload in payloads {
        content.push(payload.len() as u8);
        content.extend_from_slice(payload);
    }
    let (directory, path) =
        write_fixture("framing_length", OsString::from("records.bin"), &content);

    let output = run_os(&[
        OsString::from("partition"),
        path.as_os_str().to_owned(),
        OsString::from("--parts"),
        OsString::from("2"),
        OsString::from("--framing"),
        OsString::from("length-prefixed"),
        OsString::from("--prefix-bytes"),
        OsString::from("1"),
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        parse_ranges(&output.stdout),
        vec![(0, 0, 23, 23), (1, 23, 24, 1)]
    );

    // Big-endian two-byte prefixes and a payload-only length are equivalent
    // for small records.
    let mut be_content = Vec::new();
    for payload in payloads {
        be_content.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        be_content.extend_from_slice(payload);
    }
    let (be_directory, be_path) = write_fixture(
        "framing_length_be",
        OsString::from("records.bin"),
        &be_content,
    );
    let output = run_os(&[
        OsString::from("partition"),
        be_path.as_os_str().to_owned(),
        OsString::from("--parts"),
        OsString::from("2"),
        OsString::from("--framing"),
        OsString::from("length-prefixed"),
        OsString::from("--prefix-bytes"),
        OsString::from("2"),
        OsString::from("--prefix-endian"),
        OsString::from("be"),
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ranges = parse_ranges(&output.stdout);
    assert_eq!(ranges.last().unwrap().2, be_content.len());

    fs::remove_dir_all(directory).unwrap();
    fs::remove_dir_all(be_directory).unwrap();
}

#[test]
fn index_command_writes_sidecar_and_indexed_plan_is_verifiable() {
    let mut content = Vec::new();
    for index in 0..200u32 {
        content.extend_from_slice(format!("record-{index:04}\r\n").as_bytes());
    }
    let record_size = 13usize;
    let (directory, path) = write_fixture("index_sidecar", OsString::from("records.log"), &content);

    let output = run(&[
        std::ffi::OsStr::new("index"),
        path.as_os_str(),
        std::ffi::OsStr::new("--every"),
        std::ffi::OsStr::new("4"),
        std::ffi::OsStr::new("--delimiter-hex"),
        std::ffi::OsStr::new("0d0a"),
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let mut index_name = path.as_os_str().to_owned();
    index_name.push(".mmapidx");
    let index_path = PathBuf::from(index_name);
    assert!(index_path.is_file(), "index sidecar was not created");
    let index_text = fs::read_to_string(&index_path).unwrap();
    assert!(index_text.contains("\"schema\": \"mmap-chunker-index\""));
    assert!(index_text.contains("\"record_count\": 200"));
    assert!(index_text.contains("\"stride\": 4"));

    let plan_path = directory.join("indexed-plan.json");
    let output = run(&[
        std::ffi::OsStr::new("plan"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("8"),
        std::ffi::OsStr::new("--index"),
        index_path.as_os_str(),
        std::ffi::OsStr::new("--output"),
        plan_path.as_os_str(),
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());

    let manifest = fs::read_to_string(&plan_path).unwrap();
    assert!(manifest.contains("\"strategy\": \"indexed_records\""));
    assert!(manifest.contains("\"source_mode\": null"));
    assert!(manifest.contains("\"record_count\": 200"));
    assert!(manifest.contains(&format!(
        "\"path\": \"{}\"",
        index_path.display().to_string().replace('\\', "\\\\")
    )));

    // Structural verification with the crate's own JSON reader.
    let document = mmap_chunker_core::json::Json::parse(&manifest).unwrap();
    let ranges = document.get("ranges").unwrap().as_array().unwrap();
    assert!(!ranges.is_empty() && ranges.len() <= 8);
    let mut cursor = 0usize;
    for (position, range) in ranges.iter().enumerate() {
        let start = range.get("start").unwrap().as_u64().unwrap() as usize;
        let end = range.get("end").unwrap().as_u64().unwrap() as usize;
        assert_eq!(start, cursor, "gap or overlap at range {position}");
        assert_eq!(
            start % record_size,
            0,
            "range {position} is not record aligned"
        );
        assert!(end > start);
        if position + 1 < ranges.len() {
            assert_eq!(&content[end - 2..end], b"\r\n");
        }
        cursor = end;
    }
    assert_eq!(cursor, content.len());

    // A changed source invalidates the index-derived plan.
    let mut changed = content.clone();
    changed.extend_from_slice(b"late-record\r\n");
    fs::write(&path, &changed).unwrap();
    let output = run(&[
        std::ffi::OsStr::new("plan"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("8"),
        std::ffi::OsStr::new("--index"),
        index_path.as_os_str(),
    ]);
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn plan_command_emits_versioned_manifest_and_writes_output() {
    let (directory, path) = write_fixture(
        "plan_manifest",
        OsString::from("records.log"),
        b"a\r\nb\r\nc\r\nd\r\n",
    );

    let first = run(&[
        std::ffi::OsStr::new("plan"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("2"),
        std::ffi::OsStr::new("--delimiter-hex"),
        std::ffi::OsStr::new("0d0a"),
    ]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());
    let manifest = String::from_utf8(first.stdout.clone()).unwrap();
    assert!(manifest.contains("\"schema\": \"mmap-chunker-plan\""));
    assert!(manifest.contains("\"schema_version\": 1"));
    assert!(manifest.contains("\"delimiter_hex\": \"0d0a\""));
    assert!(manifest.contains("\"delimiter_len\": 2"));
    assert!(manifest.contains("\"source_mode\": \"mmap\""));
    assert!(manifest.contains("\"actual_partitions\": 2"));
    assert!(manifest.contains("\"requested_partitions\": 2"));
    assert!(manifest.contains("\"sample_fingerprint\": \"fnv1a64:0x"));
    assert!(manifest.contains("\"index\": 0"));
    assert!(manifest.contains(&format!("\"size\": {}", 12)));

    let second = run(&[
        std::ffi::OsStr::new("plan"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("2"),
        std::ffi::OsStr::new("--delimiter-hex"),
        std::ffi::OsStr::new("0d0a"),
    ]);
    assert_eq!(
        first.stdout, second.stdout,
        "manifest output must be deterministic"
    );

    let output_path = directory.join("plan.json");
    let written = run(&[
        std::ffi::OsStr::new("plan"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("2"),
        std::ffi::OsStr::new("--delimiter-hex"),
        std::ffi::OsStr::new("0d0a"),
        std::ffi::OsStr::new("--output"),
        output_path.as_os_str(),
    ]);
    assert!(
        written.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&written.stderr)
    );
    assert!(written.stdout.is_empty());
    let file_contents = fs::read_to_string(&output_path).unwrap();
    assert_eq!(file_contents.trim_end(), manifest.trim_end());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn plan_command_source_modes_report_backend() {
    let (directory, path) = write_fixture(
        "plan_sources",
        OsString::from("records.log"),
        b"a\r\nb\r\nc\r\nd\r\n",
    );

    for (source, window, expected_mode) in [
        ("mmap", None, "\"source_mode\": \"mmap\""),
        ("windowed", Some("65536"), "\"source_mode\": \"windowed\""),
        ("pread", None, "\"source_mode\": \"pread\""),
    ] {
        let mut arguments = vec![
            OsString::from("plan"),
            path.as_os_str().to_owned(),
            OsString::from("--parts"),
            OsString::from("2"),
            OsString::from("--delimiter-hex"),
            OsString::from("0d0a"),
            OsString::from("--source"),
            OsString::from(source),
        ];
        if let Some(window) = window {
            arguments.push(OsString::from("--window"));
            arguments.push(OsString::from(window));
        }
        let output = Command::new(binary()).args(&arguments).output().unwrap();
        assert!(
            output.status.success(),
            "source {source} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let manifest = String::from_utf8(output.stdout).unwrap();
        assert!(manifest.contains(expected_mode), "source {source}");
        assert!(
            manifest.contains("\"actual_partitions\": 2"),
            "source {source}"
        );
        let expected_window = if source == "windowed" {
            "\"window_bytes\": 65536"
        } else {
            "\"window_bytes\": null"
        };
        assert!(manifest.contains(expected_window), "source {source}");
    }

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn supports_paths_with_spaces_and_non_ascii_characters() {
    let (directory, path) = write_fixture(
        "path",
        OsString::from("records with space-ä.jsonl"),
        b"first\nsecond\nthird\n",
    );
    assert_partition_oracle(&path, "2", Some(2));
    assert_worker_projection_oracle(&path, 2, None);
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn supports_non_utf8_linux_paths() {
    use std::os::unix::ffi::OsStringExt;

    let (directory, path) = write_fixture(
        "non_utf8_path",
        OsString::from_vec(b"records-\xff.jsonl".to_vec()),
        b"first\nsecond\n",
    );
    assert_partition_oracle(&path, "2", None);
    assert_worker_projection_oracle(&path, 2, None);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn reports_invalid_invocations_to_stderr() {
    let cases: &[&[&str]] = &[
        &[],
        &["partition"],
        &["partition", "records.jsonl"],
        &["partition", "records.jsonl", "--parts", "nope"],
        &["partition", "records.jsonl", "--parts", "0"],
        &["partition", "records.jsonl", "--parts", "1", "--parts", "2"],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-byte",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-byte",
            "nope",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-byte",
            "-1",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-byte",
            "256",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-byte",
            "0x0a",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-byte",
            "10",
            "--delimiter-byte",
            "10",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
            "",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
            "0",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
            "0x0a",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
            "zz",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
            "0d 0a",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-byte",
            "10",
            "--delimiter-hex",
            "0a",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
            "0a",
            "--delimiter-byte",
            "10",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--delimiter-hex",
            "0a",
            "--delimiter-hex",
            "0a",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--source",
            "invalid",
        ],
        &["partition", "records.jsonl", "--parts", "1", "--source"],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--source",
            "mmap",
            "--source",
            "pread",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--source",
            "mmap",
            "--window",
            "65536",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--source",
            "pread",
            "--window",
            "65536",
        ],
        &["partition", "records.jsonl", "--parts", "1", "--window"],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--source",
            "windowed",
            "--window",
            "65535",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--source",
            "windowed",
            "--window",
            "nope",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--source",
            "windowed",
            "--window",
            "65536",
            "--window",
            "65536",
        ],
        &["plan"],
        &["plan", "records.jsonl"],
        &["plan", "records.jsonl", "--parts", "1", "--worker", "0"],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--output",
            "plan.json",
        ],
        &["plan", "records.jsonl", "--parts", "1", "--output"],
        &[
            "plan",
            "records.jsonl",
            "--parts",
            "1",
            "--output",
            "a.json",
            "--output",
            "b.json",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--framing",
            "fixed",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--framing",
            "nope",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--framing",
            "fixed",
            "--record-bytes",
            "16",
            "--delimiter-byte",
            "10",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--framing",
            "length-prefixed",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--framing",
            "length-prefixed",
            "--prefix-bytes",
            "0",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--framing",
            "length-prefixed",
            "--prefix-bytes",
            "9",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--prefix-bytes",
            "2",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--framing",
            "fixed",
            "--record-bytes",
            "16",
            "--prefix-bytes",
            "2",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--prefix-endian",
            "middle",
        ],
        &["index", "records.jsonl"],
        &["index", "records.jsonl", "--every", "0"],
        &["index", "records.jsonl", "--every", "2", "--parts", "2"],
        &["index", "records.jsonl", "--every", "2", "--worker", "0"],
        &["partition", "records.jsonl", "--parts", "1", "--every", "2"],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--index",
            "records.mmapidx",
        ],
        &[
            "plan",
            "records.jsonl",
            "--parts",
            "2",
            "--index",
            "records.mmapidx",
            "--delimiter-byte",
            "10",
        ],
        &[
            "plan",
            "records.jsonl",
            "--parts",
            "2",
            "--index",
            "records.mmapidx",
            "--source",
            "pread",
        ],
        &[
            "plan",
            "records.jsonl",
            "--parts",
            "2",
            "--index",
            "records.mmapidx",
            "--window",
            "65536",
        ],
        &["partition", "records.jsonl", "--parts", "1", "--worker"],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--worker",
            "nope",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--worker",
            "-1",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "1",
            "--worker",
            "0",
            "--worker",
            "0",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "8",
            "--worker",
            "8",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "8",
            "--worker",
            "9",
        ],
        &[
            "partition",
            "records.jsonl",
            "--parts",
            "8",
            "--worker",
            "3/8",
        ],
        &["partition", "records.jsonl", "--parts", "1", "extra"],
    ];
    for case in cases {
        let arguments: Vec<_> = case.iter().map(std::ffi::OsStr::new).collect();
        let output = run(&arguments);
        assert!(
            !output.status.success(),
            "case unexpectedly succeeded: {case:?}"
        );
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }

    let nonexistent = run(&[
        std::ffi::OsStr::new("partition"),
        std::ffi::OsStr::new("definitely-not-a-real-file.jsonl"),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("1"),
    ]);
    assert!(!nonexistent.status.success());
    assert!(nonexistent.stdout.is_empty());
    assert!(!nonexistent.stderr.is_empty());
}

#[test]
fn accepts_worker_before_parts() {
    let (directory, path) = write_fixture(
        "worker_order",
        OsString::from("records.jsonl"),
        b"a\nb\nc\nd\n",
    );
    let full = run(&[
        std::ffi::OsStr::new("partition"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("4"),
    ]);
    let selected = run(&[
        std::ffi::OsStr::new("partition"),
        path.as_os_str(),
        std::ffi::OsStr::new("--worker"),
        std::ffi::OsStr::new("1"),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new("4"),
    ]);
    assert!(
        full.status.success(),
        "full stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert!(
        selected.status.success(),
        "selected stderr: {}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let range = parse_ranges(&full.stdout)[1];
    assert_eq!(
        String::from_utf8(selected.stdout).unwrap(),
        format!("{}\t{}\t{}\t{}\n", range.0, range.1, range.2, range.3)
    );
    assert!(selected.stderr.is_empty());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn extreme_parts_request_remains_bounded() {
    let (directory, path) =
        write_fixture("extreme_parts", OsString::from("records.jsonl"), b"a\nb\n");
    let parts = usize::MAX.to_string();
    let output = run(&[
        std::ffi::OsStr::new("partition"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new(&parts),
        std::ffi::OsStr::new("--worker"),
        std::ffi::OsStr::new("0"),
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(parse_ranges(&output.stdout), vec![(0, 0, 2, 2)]);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn help_and_version_are_available() {
    let help = run(&[std::ffi::OsStr::new("--help")]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("Usage:"));
    assert!(help_text.contains("--delimiter-byte B"));
    assert!(help_text.contains("0..255"));
    assert!(help_text.contains("--delimiter-hex HEX"));
    assert!(help_text.contains("0d0a"));
    assert!(help_text.contains("complete record boundary"));
    assert!(help_text.contains("--source MODE"));
    assert!(help_text.contains("--window BYTES"));
    assert!(help_text.contains("--framing MODE"));
    assert!(help_text.contains("--record-bytes N"));
    assert!(help_text.contains("--prefix-bytes N"));
    assert!(help_text.contains("--prefix-endian ENDIAN"));
    assert!(help_text.contains("--length-includes-prefix"));
    assert!(help_text.contains("--every N"));
    assert!(help_text.contains("--index PATH"));
    assert!(help_text.contains("mmap-chunker index FILE"));
    assert!(help_text.contains("mmap-chunker verify MANIFEST FILE"));
    assert!(help_text.contains("verify ok"));
    assert!(help_text.contains("--output PATH"));
    assert!(help_text.contains("mmap-chunker plan FILE"));
    assert!(help_text.contains("--worker K"));
    assert!(help_text.contains("no actual partition K exists"));

    let version = run(&[std::ffi::OsStr::new("--version")]);
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("mmap-chunker "));
}

fn run_verify(manifest: &Path, file: &Path) -> Output {
    run(&[OsStr::new("verify"), manifest.as_os_str(), file.as_os_str()])
}

fn plan_to_manifest(directory: &Path, file: &Path, name: &str, extra: &[&OsStr]) -> PathBuf {
    let manifest = directory.join(name);
    let mut arguments: Vec<&OsStr> = vec![
        OsStr::new("plan"),
        file.as_os_str(),
        OsStr::new("--parts"),
        OsStr::new("4"),
        OsStr::new("--output"),
        manifest.as_os_str(),
    ];
    arguments.extend_from_slice(extra);
    let planned = run(&arguments);
    assert!(
        planned.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&planned.stderr)
    );
    manifest
}

fn ok_line(output: &Output) -> String {
    assert!(
        output.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "success must stay silent on stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// Replace the ranges array of a manifest with hand-written entries.
/// The file content (hence every record boundary) is known, so the
/// tampered manifests below do not depend on planner output details.
fn with_ranges(manifest: &str, entries: &str) -> String {
    let cut = manifest.find("\"ranges\": [").unwrap() + "\"ranges\": [".len();
    format!("{}{entries}]}}", &manifest[..cut])
}

#[test]
fn verify_command_accepts_fresh_manifest_and_is_deterministic() {
    let (directory, path) = write_fixture(
        "verify_fresh",
        OsString::from("records.log"),
        b"r01\nr02\nr03\nr04\nr05\nr06\n",
    );
    let manifest = plan_to_manifest(&directory, &path, "plan.json", &[]);

    let first = ok_line(&run_verify(&manifest, &path));
    assert!(
        first.starts_with("verify ok: "),
        "machine-readable success prefix missing: {first}"
    );
    assert!(first.contains("24 bytes"), "unexpected line: {first}");
    assert!(
        first.contains("delimiter framing (mmap-chunker-plan v1)"),
        "unexpected line: {first}"
    );
    assert_eq!(first.lines().count(), 1);

    let second = ok_line(&run_verify(&manifest, &path));
    assert_eq!(first, second, "verification must be deterministic");

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn verify_command_accepts_empty_file() {
    let (directory, path) = write_fixture("verify_empty", OsString::from("empty.log"), b"");
    let manifest = plan_to_manifest(&directory, &path, "plan.json", &[]);
    let line = ok_line(&run_verify(&manifest, &path));
    assert!(line.contains("0 ranges"), "unexpected line: {line}");
    assert!(line.contains("0 bytes"), "unexpected line: {line}");

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn verify_command_accepts_crlf_fixed_and_length_prefixed() {
    // CRLF multi-byte framing.
    let (directory, path) = write_fixture(
        "verify_crlf",
        OsString::from("crlf.log"),
        b"a\r\nb\r\nc\r\nd\r\n",
    );
    let manifest = plan_to_manifest(
        &directory,
        &path,
        "crlf.json",
        &[OsStr::new("--delimiter-hex"), OsStr::new("0d0a")],
    );
    let line = ok_line(&run_verify(&manifest, &path));
    assert!(
        line.contains("delimiter framing"),
        "unexpected line: {line}"
    );
    fs::remove_dir_all(directory).unwrap();

    // Fixed-width framing: four 8-byte records.
    let (directory, path) = write_fixture(
        "verify_fixed",
        OsString::from("fixed.bin"),
        b"12345678abcdefghIJKLMNOP",
    );
    let manifest = plan_to_manifest(
        &directory,
        &path,
        "fixed.json",
        &[
            OsStr::new("--framing"),
            OsStr::new("fixed"),
            OsStr::new("--record-bytes"),
            OsStr::new("8"),
        ],
    );
    let line = ok_line(&run_verify(&manifest, &path));
    assert!(
        line.contains("fixed_width framing"),
        "unexpected line: {line}"
    );
    fs::remove_dir_all(directory).unwrap();

    // Length-prefixed framing: two records with u32LE length prefixes.
    let mut content = Vec::new();
    for payload in [b"abc".as_slice(), b"defghi".as_slice()] {
        content.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        content.extend_from_slice(payload);
    }
    let (directory, path) = write_fixture(
        "verify_length_prefixed",
        OsString::from("framed.bin"),
        &content,
    );
    let manifest = plan_to_manifest(
        &directory,
        &path,
        "framed.json",
        &[
            OsStr::new("--framing"),
            OsStr::new("length-prefixed"),
            OsStr::new("--prefix-bytes"),
            OsStr::new("4"),
        ],
    );
    let line = ok_line(&run_verify(&manifest, &path));
    assert!(
        line.contains("length_prefixed framing"),
        "unexpected line: {line}"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn verify_command_accepts_indexed_plan() {
    let mut content = Vec::new();
    for index in 0..64u32 {
        content.extend_from_slice(format!("record-{index:04}\n").as_bytes());
    }
    let (directory, path) =
        write_fixture("verify_indexed", OsString::from("records.log"), &content);
    let indexed = run(&[
        OsStr::new("index"),
        path.as_os_str(),
        OsStr::new("--every"),
        OsStr::new("8"),
    ]);
    assert!(indexed.status.success());
    let mut index_name = path.as_os_str().to_owned();
    index_name.push(".mmapidx");
    let manifest = directory.join("indexed.json");
    let planned = run(&[
        OsStr::new("plan"),
        path.as_os_str(),
        OsStr::new("--parts"),
        OsStr::new("4"),
        OsStr::new("--index"),
        Path::new(&index_name).as_os_str(),
        OsStr::new("--output"),
        manifest.as_os_str(),
    ]);
    assert!(planned.status.success());
    let line = ok_line(&run_verify(&manifest, &path));
    assert!(line.starts_with("verify ok: "), "unexpected line: {line}");

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn verify_command_rejects_tampered_manifest() {
    // Six 4-byte records: every 4-byte offset is a boundary, so the
    // tampered splits below are unambiguous by construction.
    let content = b"r01\nr02\nr03\nr04\nr05\nr06\n";
    let (directory, path) =
        write_fixture("verify_tampered", OsString::from("records.log"), content);
    let manifest = plan_to_manifest(&directory, &path, "plan.json", &[]);
    let text = fs::read_to_string(&manifest).unwrap();

    let entry = |index: usize, start: usize, end: usize| {
        format!(
            "{{\"index\": {index}, \"start\": {start}, \"end\": {end}, \"length\": {}}}",
            end - start
        )
    };
    let case = |entries: &str, kind: &str| {
        let tampered_path = directory.join(format!("tampered-{kind}.json"));
        fs::write(&tampered_path, with_ranges(&text, entries)).unwrap();
        let output = run_verify(&tampered_path, &path);
        assert!(
            !output.status.success(),
            "{kind} tampering verified unexpectedly"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(&format!("verify failed [{kind}]")),
            "stable kind token [{kind}] missing in: {stderr}"
        );
    };

    case(
        &format!(
            "{},{},{}",
            entry(0, 0, 8),
            entry(1, 7, 16),
            entry(2, 16, 24)
        ),
        "overlap",
    );
    case(
        &format!(
            "{},{},{}",
            entry(0, 0, 8),
            entry(1, 9, 16),
            entry(2, 16, 24)
        ),
        "gap",
    );
    case(
        &format!(
            "{},{},{}",
            entry(0, 0, 7),
            entry(1, 7, 16),
            entry(2, 16, 24)
        ),
        "misaligned",
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn verify_command_rejects_changed_source() {
    // 200 KiB of 5-byte records: a flip at offset 100_000 lies outside
    // both 64 KiB fingerprint samples, which is exactly the blind spot
    // the honesty case below must demonstrate.
    let content: Vec<u8> = b"abcd\n".repeat(41_943 + 1);
    let content = &content[..204_800];
    assert_eq!(content.len(), 204_800);
    assert_ne!(content[100_000], b'\n');
    let (directory, path) = write_fixture("verify_changed", OsString::from("records.log"), content);
    let manifest = plan_to_manifest(&directory, &path, "plan.json", &[]);

    // Appended bytes change the size: the manifest is stale.
    let mut grown = content.to_vec();
    grown.extend_from_slice(b"abcd\n");
    fs::write(&path, &grown).unwrap();
    let output = run_verify(&manifest, &path);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("stale manifest"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Same size, changed head bytes: the sampled fingerprint catches it.
    let mut headed = content.to_vec();
    headed[0] = b'X';
    fs::write(&path, &headed).unwrap();
    let output = run_verify(&manifest, &path);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("source content changed"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Same size, in-record middle edit outside the fingerprint samples
    // with preserved boundaries: coverage still holds. This passing
    // result documents the honest limit — coverage is not content
    // authentication.
    let mut middled = content.to_vec();
    middled[100_000] = b'X';
    fs::write(&path, &middled).unwrap();
    let line = ok_line(&run_verify(&manifest, &path));
    assert!(line.starts_with("verify ok: "));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn verify_command_rejects_bad_inputs() {
    let (directory, path) = write_fixture(
        "verify_inputs",
        OsString::from("records.log"),
        b"r01\nr02\n",
    );
    let manifest = plan_to_manifest(&directory, &path, "plan.json", &[]);

    // Missing manifest file.
    let output = run_verify(&directory.join("absent.json"), &path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to read manifest"));

    // Missing source file.
    let output = run_verify(&manifest, &directory.join("absent.log"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to identify"));

    // Empty and truncated manifests are not JSON.
    let empty = directory.join("empty.json");
    fs::write(&empty, b"").unwrap();
    let output = run_verify(&empty, &path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid manifest (invalid_json)"));

    let text = fs::read_to_string(&manifest).unwrap();
    let truncated = directory.join("truncated.json");
    fs::write(&truncated, &text[..text.len() / 2]).unwrap();
    let output = run_verify(&truncated, &path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid manifest (invalid_json)"));

    // Wrong arity is a usage error, not a verification verdict.
    let output = run(&[OsStr::new("verify"), manifest.as_os_str()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));

    fs::remove_dir_all(directory).unwrap();
}
