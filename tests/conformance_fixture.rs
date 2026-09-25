//! Regression guard for the byte-exact conformance fixture.
//!
//! `tests/conformance/corpus.jsonl` pins LF-derived values in
//! `tests/conformance/expected.txt`. A CRLF checkout (Windows autocrlf
//! without protection) silently breaks all four conformance consumers:
//! 152 bytes become 156 and every derived length/hash mismatches.
//! `.gitattributes` pins this fixture to `text eol=lf`; this test fails
//! closed if the file on disk ever contains carriage returns again.

use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("conformance")
        .join("corpus.jsonl")
}

#[test]
fn conformance_corpus_has_unix_line_endings() {
    let bytes = std::fs::read(fixture_path()).expect("conformance corpus must be readable");
    assert!(!bytes.is_empty(), "conformance corpus must not be empty");
    assert!(
        !bytes.contains(&b'\r'),
        "conformance corpus must not contain CR bytes (got {} bytes, check .gitattributes eol=lf)",
        bytes.len()
    );
}
