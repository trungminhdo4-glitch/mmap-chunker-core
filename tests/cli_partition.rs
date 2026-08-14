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
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn supports_non_utf8_unix_paths() {
    use std::os::unix::ffi::OsStringExt;

    let (directory, path) = write_fixture(
        "non_utf8_path",
        OsString::from_vec(b"records-\xff.jsonl".to_vec()),
        b"first\nsecond\n",
    );
    assert_partition_oracle(&path, "2", None);
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
fn help_and_version_are_available() {
    let help = run(&[std::ffi::OsStr::new("--help")]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));

    let version = run(&[std::ffi::OsStr::new("--version")]);
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("mmap-chunker "));
}
