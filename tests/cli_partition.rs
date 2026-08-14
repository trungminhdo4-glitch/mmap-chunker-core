use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

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

fn assert_worker_projection_oracle(path: &Path, parts: usize) {
    let parts_text = parts.to_string();
    let full = run(&[
        std::ffi::OsStr::new("partition"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new(&parts_text),
    ]);
    assert!(
        full.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert!(full.stderr.is_empty());
    let ranges = parse_ranges(&full.stdout);

    for worker in 0..parts {
        let worker_text = worker.to_string();
        let selected = run(&[
            std::ffi::OsStr::new("partition"),
            path.as_os_str(),
            std::ffi::OsStr::new("--parts"),
            std::ffi::OsStr::new(&parts_text),
            std::ffi::OsStr::new("--worker"),
            std::ffi::OsStr::new(&worker_text),
        ]);
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
    let first = run(&[
        std::ffi::OsStr::new("partition"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new(parts),
    ]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stderr.is_empty());
    let second = run(&[
        std::ffi::OsStr::new("partition"),
        path.as_os_str(),
        std::ffi::OsStr::new("--parts"),
        std::ffi::OsStr::new(parts),
    ]);
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
            assert_eq!(source[end - 1], b'\n', "range {index} split a record");
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
        assert_worker_projection_oracle(&path, parts.parse().unwrap());
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn supports_paths_with_spaces_and_non_ascii_characters() {
    let (directory, path) = write_fixture(
        "path",
        OsString::from("records with space-ä.jsonl"),
        b"first\nsecond\nthird\n",
    );
    assert_partition_oracle(&path, "2", Some(2));
    assert_worker_projection_oracle(&path, 2);
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
    assert_worker_projection_oracle(&path, 2);
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
    assert!(help_text.contains("--worker K"));
    assert!(help_text.contains("no actual partition K exists"));

    let version = run(&[std::ffi::OsStr::new("--version")]);
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("mmap-chunker "));
}
