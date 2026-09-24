//! Versioned, reproducible range-plan manifests.
//!
//! A plan captures everything needed to execute a set of record-aligned
//! byte ranges independently: source identity, framing, partitioning
//! parameters, and the ranges themselves. The manifest is the artifact
//! that is handed to workers — mmap-chunker plans, other systems
//! schedule.
//!
//! The JSON schema is intentionally small and stable:
//!
//! ```json
//! {
//!   "schema": "mmap-chunker-plan",
//!   "schema_version": 1,
//!   "planner": { "name": "mmap-chunker-core", "version": "0.2.2" },
//!   "source": {
//!     "path": "huge.jsonl",
//!     "size": 123,
//!     "identity": {
//!       "modified_unix_nanos": 123456789,
//!       "device": 12,
//!       "inode": 34,
//!       "sample_fingerprint": "fnv1a64:0x0000000000000000",
//!       "sample_bytes": 65536
//!     }
//!   },
//!   "framing": { "strategy": "delimiter", "delimiter_hex": "0d0a", "delimiter_len": 2 },
//!   "partitioning": {
//!     "strategy": "bytes",
//!     "requested_partitions": 8,
//!     "actual_partitions": 8,
//!     "source_mode": "mmap",
//!     "window_bytes": null
//!   },
//!   "ranges": [ { "index": 0, "start": 0, "end": 123, "length": 123 } ]
//! }
//! ```
//!
//! Identity fields are `null` when the platform does not expose them.
//! Consumers should verify every non-null field before using a plan.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::framing::{BuiltinFraming, FramingDescriptor, FramingStrategy};
use crate::source::{
    plan_partition_ranges_with, ByteSource, PlannerOptions, SourceMode, DEFAULT_SCAN_BUFFER_BYTES,
};

/// Manifest schema name.
pub const PLAN_SCHEMA: &str = "mmap-chunker-plan";

/// Manifest schema version.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// Number of bytes sampled from the head and tail of a file for the
/// content fingerprint.
pub const IDENTITY_SAMPLE_BYTES: usize = 64 * 1024;

const FNV1A64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;
const FNV1A64_PREFIX: &str = "fnv1a64:0x";

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

/// Best-effort platform file ids: `(device, inode)` on Unix, or
/// `(volume serial number, file index)` on Windows.
///
/// Returns `(None, None)` when the platform call is unavailable; the
/// size/mtime/fingerprint fields still seal the identity.
#[cfg(unix)]
fn platform_file_ids(path: &Path) -> io::Result<(Option<u64>, Option<u64>)> {
    let metadata = std::fs::metadata(path)?;
    Ok((Some(metadata.dev()), Some(metadata.ino())))
}

#[cfg(windows)]
fn platform_file_ids(path: &Path) -> io::Result<(Option<u64>, Option<u64>)> {
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time_low: u32,
        creation_time_high: u32,
        last_access_time_low: u32,
        last_access_time_high: u32,
        last_write_time_low: u32,
        last_write_time_high: u32,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    extern "system" {
        fn GetFileInformationByHandle(
            file: isize,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    let file = File::open(path)?;
    let mut information = ByHandleFileInformation::default();
    // SAFETY: `file` owns a valid handle for the duration of the call
    // and `information` is a correctly laid out writable struct.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as isize, &mut information) };
    if ok == 0 {
        return Ok((None, None));
    }

    let device = u64::from(information.volume_serial_number);
    let inode =
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
    Ok((Some(device), Some(inode)))
}

/// Cheap, portable file identity.
///
/// `size`, `modified_unix_nanos`, `device`, and `inode` come from file
/// metadata. `sample_fingerprint` hashes the size plus up to 64 KiB
/// from the head and 64 KiB from the tail so content changes that
/// preserve size and mtime are still detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    /// File size in bytes.
    pub size: u64,
    /// Modification time as nanoseconds since the Unix epoch.
    pub modified_unix_nanos: Option<u128>,
    /// Device id (Unix) or 32-bit volume serial number (Windows).
    ///
    /// CPython >= 3.12 reports the 64-bit `FileIdInfo` serial in
    /// `st_dev` on Windows; its low 32 bits equal this value, so the
    /// Python verifier compares that half.
    pub device: Option<u64>,
    /// Inode number (Unix) or file index (Windows).
    pub inode: Option<u64>,
    /// `fnv1a64:0x...` fingerprint over size and sampled bytes.
    pub sample_fingerprint: Option<String>,
    /// Number of bytes sampled from each end of the file.
    pub sample_bytes: usize,
}

/// Reference to the sparse record index a plan was derived from.
///
/// Plans built from an index do not scan the source; the recorded
/// stride and record count make the derivation reproducible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReference {
    /// Every `stride`-th record start was recorded in the index.
    pub stride: u64,
    /// Total record count observed when the index was built.
    pub record_count: u64,
    /// Index file path, when the plan was produced from a file.
    pub path: Option<String>,
}

/// A complete, reproducible range plan for one immutable file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangePlan {
    /// Source path as recorded at planning time.
    pub source_path: String,
    /// Source size in bytes.
    pub source_size: u64,
    /// Identity captured immediately before and verified after planning.
    pub identity: FileIdentity,
    /// Record framing used for the plan.
    pub framing: FramingDescriptor,
    /// Number of partitions requested.
    pub requested_partitions: usize,
    /// Backend used to read the source while planning; `None` for
    /// index-derived plans, which do not scan the source.
    pub source_mode: Option<SourceMode>,
    /// Window size if `source_mode == Some(Windowed)`.
    pub window_bytes: Option<usize>,
    /// Sparse index reference for index-derived plans.
    pub index: Option<IndexReference>,
    /// Record-aligned `(start, end)` ranges covering the file exactly.
    pub ranges: Vec<(usize, usize)>,
}

/// Compute the FNV-1a 64 fingerprint of `size` (little-endian) followed
/// by the sample bytes.
fn fingerprint_bytes(size: u64, samples: &[&[u8]]) -> String {
    let mut hash = FNV1A64_OFFSET_BASIS;
    let mut update = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV1A64_PRIME);
        }
    };
    update(&size.to_le_bytes());
    for sample in samples {
        update(sample);
    }
    format!("{FNV1A64_PREFIX}{hash:016x}")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(out, offset)
}

#[cfg(windows)]
fn read_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(out, offset)
}

fn sample_fingerprint(path: &Path, size: u64) -> io::Result<String> {
    if size == 0 {
        return Ok(fingerprint_bytes(0, &[]));
    }

    let file = File::open(path)?;
    let sample = u64::try_from(IDENTITY_SAMPLE_BYTES).unwrap_or(u64::MAX);
    let head_len = size.min(sample);
    let tail_len = size.min(sample);

    let mut head = vec![0u8; usize::try_from(head_len).unwrap_or(0)];
    let head_read = read_at(&file, &mut head, 0)?;
    head.truncate(head_read);

    // When head and tail overlap, hash the contiguous file once.
    if head_len + tail_len >= size {
        let needed = usize::try_from(size).unwrap_or(head.len());
        if needed != head.len() {
            head.resize(needed, 0);
            let mut total = head_read;
            while total < needed {
                let read = read_at(&file, &mut head[total..], total as u64)?;
                if read == 0 {
                    break;
                }
                total += read;
            }
            head.truncate(total);
        }
        return Ok(fingerprint_bytes(size, &[&head]));
    }

    let mut tail = vec![0u8; usize::try_from(tail_len).unwrap_or(0)];
    let tail_read = read_at(&file, &mut tail, size - tail_len)?;
    tail.truncate(tail_read);

    Ok(fingerprint_bytes(size, &[&head, &tail]))
}

/// Collect the identity of the file at `path`.
pub fn identify_file(path: impl AsRef<Path>) -> io::Result<FileIdentity> {
    let path = path.as_ref();
    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();

    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());

    let (device, inode) = platform_file_ids(path)?;

    Ok(FileIdentity {
        size,
        modified_unix_nanos,
        device,
        inode,
        sample_fingerprint: Some(sample_fingerprint(path, size)?),
        sample_bytes: IDENTITY_SAMPLE_BYTES,
    })
}
/// Plan range partitions for `path` using a raw delimiter and seal the
/// result with a file identity.
///
/// Convenience wrapper around [`plan_file_with_framing`].
///
/// # Safety
///
/// The caller must ensure that the file is not modified while this
/// function runs (see [`plan_partition_ranges`]).
pub unsafe fn plan_file(
    path: impl AsRef<Path>,
    requested_partitions: usize,
    delimiter: &[u8],
    source_mode: SourceMode,
    window_bytes: usize,
) -> io::Result<RangePlan> {
    let strategy = BuiltinFraming::delimiter(delimiter.to_vec())?;
    unsafe {
        plan_file_with_framing(
            path,
            requested_partitions,
            &strategy,
            source_mode,
            window_bytes,
        )
    }
}

/// Plan range partitions for `path` using a pluggable framing strategy
/// and seal the result with a file identity captured before and
/// re-checked after planning.
///
/// Returns an error (without producing a plan) if the file identity
/// changes while planning, so a manifest can never describe mixed
/// content.
///
/// # Safety
///
/// The caller must ensure that the file is not modified while this
/// function runs (see [`plan_partition_ranges_with`]).
pub unsafe fn plan_file_with_framing(
    path: impl AsRef<Path>,
    requested_partitions: usize,
    strategy: &dyn FramingStrategy,
    source_mode: SourceMode,
    window_bytes: usize,
) -> io::Result<RangePlan> {
    let path = path.as_ref();
    let identity_before = identify_file(path)?;

    let options = PlannerOptions {
        mode: source_mode,
        window_bytes,
        scan_buffer_bytes: DEFAULT_SCAN_BUFFER_BYTES,
    };
    let ranges =
        unsafe { plan_partition_ranges_with(path, requested_partitions, strategy, &options)? };

    let identity_after = identify_file(path)?;
    if identity_before != identity_after {
        return Err(io::Error::other(
            "source file changed while planning; plan rejected",
        ));
    }

    Ok(RangePlan {
        source_path: path.to_string_lossy().into_owned(),
        source_size: identity_before.size,
        identity: identity_before,
        framing: strategy.describe(),
        requested_partitions,
        source_mode: Some(source_mode),
        window_bytes: match source_mode {
            SourceMode::Windowed => Some(window_bytes),
            _ => None,
        },
        index: None,
        ranges,
    })
}

/// Build a [`RangePlan`] from precomputed sparse-index ranges.
///
/// The identity is re-checked against the current file, and the supplied
/// ranges must exactly cover the file; callers are responsible for
/// deriving record-aligned ranges from a verified index.
pub fn plan_from_ranges(
    path: impl AsRef<Path>,
    identity: &FileIdentity,
    framing: FramingDescriptor,
    requested_partitions: usize,
    index: IndexReference,
    ranges: Vec<(usize, usize)>,
) -> io::Result<RangePlan> {
    let path = path.as_ref();
    let current = identify_file(path)?;
    if &current != identity {
        return Err(io::Error::other(
            "source identity does not match the plan inputs; plan rejected",
        ));
    }
    if current.size != identity.size {
        return Err(io::Error::other("source size changed; plan rejected"));
    }

    Ok(RangePlan {
        source_path: path.to_string_lossy().into_owned(),
        source_size: identity.size,
        identity: identity.clone(),
        framing,
        requested_partitions,
        source_mode: None,
        window_bytes: None,
        index: Some(index),
        ranges,
    })
}

pub(crate) fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn push_option_u128(out: &mut String, value: Option<u128>) {
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("null"),
    }
}

fn push_option_u64(out: &mut String, value: Option<u64>) {
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("null"),
    }
}

pub(crate) fn push_option_string(out: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_json_string(out, value),
        None => out.push_str("null"),
    }
}

fn delimiter_hex(delimiter: &[u8]) -> String {
    let mut out = String::with_capacity(delimiter.len() * 2);
    for byte in delimiter {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let pair = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(out)
}

/// Write a framing descriptor as a JSON object using `indent` for the
/// object itself (members are indented one level deeper).
pub(crate) fn write_framing_json(out: &mut String, framing: &FramingDescriptor, indent: &str) {
    let member = format!("{indent}  ");
    out.push_str("{\n");
    match framing {
        FramingDescriptor::Delimiter { delimiter } => {
            out.push_str(&member);
            out.push_str("\"strategy\": \"delimiter\",\n");
            out.push_str(&member);
            out.push_str("\"delimiter_hex\": ");
            push_json_string(out, &delimiter_hex(delimiter));
            out.push_str(",\n");
            out.push_str(&member);
            out.push_str(&format!("\"delimiter_len\": {}\n", delimiter.len()));
        }
        FramingDescriptor::FixedWidth { record_bytes } => {
            out.push_str(&member);
            out.push_str("\"strategy\": \"fixed_width\",\n");
            out.push_str(&member);
            out.push_str(&format!("\"record_bytes\": {record_bytes}\n"));
        }
        FramingDescriptor::LengthPrefixed {
            prefix_bytes,
            little_endian,
            length_includes_prefix,
        } => {
            out.push_str(&member);
            out.push_str("\"strategy\": \"length_prefixed\",\n");
            out.push_str(&member);
            out.push_str(&format!("\"prefix_bytes\": {prefix_bytes},\n"));
            out.push_str(&member);
            out.push_str(&format!("\"little_endian\": {little_endian},\n"));
            out.push_str(&member);
            out.push_str(&format!(
                "\"length_includes_prefix\": {length_includes_prefix}\n"
            ));
        }
        FramingDescriptor::Custom { name } => {
            out.push_str(&member);
            out.push_str("\"strategy\": \"custom\",\n");
            out.push_str(&member);
            out.push_str("\"name\": ");
            push_json_string(out, name);
            out.push('\n');
        }
    }
    out.push_str(indent);
    out.push('}');
}

/// Parse a framing descriptor produced by [`write_framing_json`].
pub(crate) fn framing_descriptor_from_json(value: &crate::json::Json) -> Option<FramingDescriptor> {
    let strategy = value.get("strategy")?.as_str()?;
    match strategy {
        "delimiter" => {
            let delimiter = decode_hex(value.get("delimiter_hex")?.as_str()?)?;
            if delimiter.is_empty() {
                return None;
            }
            Some(FramingDescriptor::Delimiter { delimiter })
        }
        "fixed_width" => Some(FramingDescriptor::FixedWidth {
            record_bytes: usize::try_from(value.get("record_bytes")?.as_u64()?).ok()?,
        }),
        "length_prefixed" => Some(FramingDescriptor::LengthPrefixed {
            prefix_bytes: u8::try_from(value.get("prefix_bytes")?.as_u64()?).ok()?,
            little_endian: value.get("little_endian")?.as_bool()?,
            length_includes_prefix: value.get("length_includes_prefix")?.as_bool()?,
        }),
        "custom" => Some(FramingDescriptor::Custom {
            name: value.get("name")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

fn optional_json_u128(value: &crate::json::Json) -> Option<Option<u128>> {
    match value {
        crate::json::Json::Null => Some(None),
        other => Some(Some(other.as_u128()?)),
    }
}

fn optional_json_u64(value: &crate::json::Json) -> Option<Option<u64>> {
    match value {
        crate::json::Json::Null => Some(None),
        other => Some(Some(other.as_u64()?)),
    }
}

/// Parse a `source` object produced by [`RangePlan::to_json`] or the
/// index writer into a [`FileIdentity`].
pub(crate) fn file_identity_from_json(source: &crate::json::Json) -> Option<FileIdentity> {
    let size = source.get("size")?.as_u64()?;
    let identity = source.get("identity")?;
    Some(FileIdentity {
        size,
        modified_unix_nanos: optional_json_u128(identity.get("modified_unix_nanos")?)?,
        device: optional_json_u64(identity.get("device")?)?,
        inode: optional_json_u64(identity.get("inode")?)?,
        sample_fingerprint: match identity.get("sample_fingerprint")? {
            crate::json::Json::Null => None,
            value => Some(value.as_str()?.to_owned()),
        },
        sample_bytes: usize::try_from(identity.get("sample_bytes")?.as_u64()?).ok()?,
    })
}

impl FileIdentity {
    pub(crate) fn write_json(&self, out: &mut String, indent: &str) {
        out.push_str("{\n");
        out.push_str(indent);
        out.push_str("  \"modified_unix_nanos\": ");
        push_option_u128(out, self.modified_unix_nanos);
        out.push_str(",\n");
        out.push_str(indent);
        out.push_str("  \"device\": ");
        push_option_u64(out, self.device);
        out.push_str(",\n");
        out.push_str(indent);
        out.push_str("  \"inode\": ");
        push_option_u64(out, self.inode);
        out.push_str(",\n");
        out.push_str(indent);
        out.push_str("  \"sample_fingerprint\": ");
        push_option_string(out, self.sample_fingerprint.as_deref());
        out.push_str(",\n");
        out.push_str(indent);
        out.push_str(&format!("  \"sample_bytes\": {}\n", self.sample_bytes));
        out.push_str(indent);
        out.push('}');
    }
}

impl RangePlan {
    /// Render the plan as deterministic, pretty-printed JSON.
    ///
    /// Key order, formatting, and range order are fixed; the same plan
    /// always produces byte-identical output.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str("  \"schema\": ");
        push_json_string(&mut out, PLAN_SCHEMA);
        out.push_str(",\n");
        out.push_str(&format!("  \"schema_version\": {PLAN_SCHEMA_VERSION},\n"));
        out.push_str("  \"planner\": { \"name\": \"mmap-chunker-core\", \"version\": ");
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

        out.push_str("  \"partitioning\": {\n");
        let strategy = if self.index.is_some() {
            "indexed_records"
        } else {
            "bytes"
        };
        out.push_str(&format!("    \"strategy\": \"{strategy}\",\n"));
        out.push_str(&format!(
            "    \"requested_partitions\": {},\n",
            self.requested_partitions
        ));
        out.push_str(&format!(
            "    \"actual_partitions\": {},\n",
            self.ranges.len()
        ));
        out.push_str("    \"source_mode\": ");
        match self.source_mode {
            Some(mode) => push_json_string(&mut out, mode.as_str()),
            None => out.push_str("null"),
        }
        out.push_str(",\n");
        out.push_str("    \"window_bytes\": ");
        match self.window_bytes {
            Some(window) => out.push_str(&window.to_string()),
            None => out.push_str("null"),
        }
        out.push_str(",\n");
        out.push_str("    \"source_index\": ");
        match &self.index {
            Some(index) => {
                out.push_str("{\n");
                out.push_str("      \"path\": ");
                push_option_string(&mut out, index.path.as_deref());
                out.push_str(",\n");
                out.push_str(&format!("      \"stride\": {},\n", index.stride));
                out.push_str(&format!("      \"record_count\": {}\n", index.record_count));
                out.push_str("    }\n");
            }
            None => out.push_str("null\n"),
        }
        out.push_str("  },\n");

        out.push_str("  \"ranges\": [");
        if self.ranges.is_empty() {
            out.push_str("]\n}");
            return out;
        }
        out.push('\n');
        for (index, &(start, end)) in self.ranges.iter().enumerate() {
            out.push_str(&format!(
                "    {{ \"index\": {index}, \"start\": {start}, \"end\": {end}, \"length\": {} }}",
                end - start
            ));
            if index + 1 < self.ranges.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}");
        out
    }

    /// Write [`to_json`](Self::to_json) to `path`.
    pub fn write_json(&self, path: impl AsRef<Path>) -> io::Result<()> {
        std::fs::write(path, self.to_json())
    }
}

/// Maximum accepted manifest input size.
///
/// Bounds the DOM allocation of [`Json::parse`]: every parsed value is
/// backed by the input text, so the total allocation stays proportional
/// to this cap plus a small constant per value.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

/// Maximum accepted range count.
///
/// Legitimate plans carry partition-count ranges (a handful per file),
/// so this bound is orders of magnitude above real use while keeping
/// the reconstructed [`Vec`] under ~16 MiB.
pub const MAX_MANIFEST_RANGES: usize = 1_000_000;

/// Machine-readable reason a manifest failed to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestErrorKind {
    /// Input exceeds [`MAX_MANIFEST_BYTES`].
    TooLarge,
    /// Malformed JSON (with byte position in the detail).
    InvalidJson,
    /// Well-formed JSON that is not a usable plan manifest.
    InvalidSchema,
    /// Recognized manifest with an unsupported `schema_version`.
    UnsupportedVersion,
    /// More ranges than [`MAX_MANIFEST_RANGES`].
    TooManyRanges,
}

impl ManifestErrorKind {
    /// Stable lowercase name for logs and CLI diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::InvalidJson => "invalid_json",
            Self::InvalidSchema => "invalid_schema",
            Self::UnsupportedVersion => "unsupported_version",
            Self::TooManyRanges => "too_many_ranges",
        }
    }
}

/// Fail-closed manifest parse verdict for [`RangePlan::from_json`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    /// Machine-readable failure reason.
    pub kind: ManifestErrorKind,
    /// Human-readable explanation; never replaces [`Self::kind`].
    pub detail: String,
}

impl ManifestError {
    fn new(kind: ManifestErrorKind, detail: String) -> Self {
        Self { kind, detail }
    }

    fn schema(detail: String) -> Self {
        Self::new(ManifestErrorKind::InvalidSchema, detail)
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid manifest ({}): {}",
            self.kind.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for ManifestError {}

fn manifest_members<'a>(
    value: &'a crate::json::Json,
    context: &str,
) -> Result<&'a Vec<(String, crate::json::Json)>, ManifestError> {
    match value {
        crate::json::Json::Object(members) => {
            for (outer, (outer_key, _)) in members.iter().enumerate() {
                for (inner_key, _) in &members[..outer] {
                    if inner_key == outer_key {
                        return Err(ManifestError::schema(format!(
                            "{context} has a duplicate key `{outer_key}`; \
                             ambiguous manifests are rejected"
                        )));
                    }
                }
            }
            Ok(members)
        }
        _ => Err(ManifestError::schema(format!(
            "{context} must be a JSON object"
        ))),
    }
}

fn manifest_field<'a>(
    value: &'a crate::json::Json,
    context: &str,
    key: &str,
) -> Result<&'a crate::json::Json, ManifestError> {
    match value {
        crate::json::Json::Object(members) => members
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, field)| field)
            .ok_or_else(|| {
                ManifestError::schema(format!("{context} is missing required field `{key}`"))
            }),
        _ => Err(ManifestError::schema(format!(
            "{context} must be a JSON object"
        ))),
    }
}

fn manifest_u64(value: &crate::json::Json, context: &str, key: &str) -> Result<u64, ManifestError> {
    manifest_field(value, context, key)?
        .as_u64()
        .ok_or_else(|| {
            ManifestError::schema(format!(
                "{context} field `{key}` must be an unsigned integer"
            ))
        })
}

fn manifest_usize(
    value: &crate::json::Json,
    context: &str,
    key: &str,
) -> Result<usize, ManifestError> {
    let raw = manifest_u64(value, context, key)?;
    usize::try_from(raw).map_err(|_| {
        ManifestError::schema(format!(
            "{context} field `{key}` value {raw} does not fit on this platform"
        ))
    })
}

fn manifest_string(
    value: &crate::json::Json,
    context: &str,
    key: &str,
) -> Result<String, ManifestError> {
    manifest_field(value, context, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            ManifestError::schema(format!("{context} field `{key}` must be a JSON string"))
        })
}

fn parse_index_reference(
    value: &crate::json::Json,
) -> Result<Option<IndexReference>, ManifestError> {
    const CONTEXT: &str = "partitioning source_index";
    match value {
        crate::json::Json::Null => Ok(None),
        _ => {
            manifest_members(value, CONTEXT)?;
            Ok(Some(IndexReference {
                stride: manifest_u64(value, CONTEXT, "stride")?,
                record_count: manifest_u64(value, CONTEXT, "record_count")?,
                path: match manifest_field(value, CONTEXT, "path")? {
                    crate::json::Json::Null => None,
                    field => Some(
                        field
                            .as_str()
                            .ok_or_else(|| {
                                ManifestError::schema(format!(
                                    "{CONTEXT} field `path` must be a string or null"
                                ))
                            })?
                            .to_owned(),
                    ),
                },
            }))
        }
    }
}

fn parse_source_mode(value: &crate::json::Json) -> Result<Option<SourceMode>, ManifestError> {
    const CONTEXT: &str = "partitioning source_mode";
    match value {
        crate::json::Json::Null => Ok(None),
        crate::json::Json::String(name) => match name.as_str() {
            "mmap" => Ok(Some(SourceMode::Mmap)),
            "windowed" => Ok(Some(SourceMode::Windowed)),
            "pread" => Ok(Some(SourceMode::Pread)),
            _ => Err(ManifestError::schema(format!(
                "{CONTEXT} has unknown mode `{name}` (expected mmap, windowed, pread, or null)"
            ))),
        },
        _ => Err(ManifestError::schema(format!(
            "{CONTEXT} must be a string or null"
        ))),
    }
}

impl RangePlan {
    /// Reconstruct a plan from its [`to_json`](Self::to_json) document.
    ///
    /// Strict and fail-closed: unknown fields are ignored for forward
    /// compatibility, but duplicate keys in any interpreted object are
    /// rejected (the underlying [`Json::get`] lookup is first-wins, so
    /// duplicates would otherwise be silently ambiguous). Derived or
    /// informational fields (`partitioning.strategy`,
    /// `partitioning.actual_partitions`, `planner.*`) are ignored: the
    /// ranges, framing, and identity are authoritative and every
    /// structural property is re-derived by
    /// [`verify_coverage`](Self::verify_coverage).
    ///
    /// Equivalence is semantic, not byte-level: JSON admits several
    /// byte forms for equal values (whitespace, `\u` escapes, key
    /// order). Re-serializing the result with [`to_json`](Self::to_json)
    /// reproduces the canonical bytes.
    ///
    /// Limits: inputs over [`MAX_MANIFEST_BYTES`] and plans with more
    /// than [`MAX_MANIFEST_RANGES`] ranges are rejected before
    /// allocating proportional structures. No panics on untrusted
    /// input; every violation returns a [`ManifestError`].
    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::new(
                ManifestErrorKind::TooLarge,
                format!(
                    "manifest is {} bytes, limit is {MAX_MANIFEST_BYTES}",
                    text.len()
                ),
            ));
        }
        let document = crate::json::Json::parse(text).map_err(|error| {
            ManifestError::new(ManifestErrorKind::InvalidJson, error.to_string())
        })?;
        const CONTEXT: &str = "manifest";
        manifest_members(&document, CONTEXT)?;

        let schema = manifest_string(&document, CONTEXT, "schema")?;
        if schema != PLAN_SCHEMA {
            return Err(ManifestError::schema(format!(
                "unsupported schema `{schema}` (expected `{PLAN_SCHEMA}`)"
            )));
        }
        let version = manifest_field(&document, CONTEXT, "schema_version")?
            .as_u64()
            .ok_or_else(|| {
                ManifestError::schema(
                    "manifest field `schema_version` must be an unsigned integer".to_owned(),
                )
            })?;
        if version != u64::from(PLAN_SCHEMA_VERSION) {
            return Err(ManifestError::new(
                ManifestErrorKind::UnsupportedVersion,
                format!(
                    "unsupported schema_version {version} (supported: {})",
                    PLAN_SCHEMA_VERSION
                ),
            ));
        }

        let source = manifest_field(&document, CONTEXT, "source")?;
        manifest_members(source, "source")?;
        let source_path = manifest_string(source, "source", "path")?;
        let source_size = manifest_u64(source, "source", "size")?;
        let identity_value = manifest_field(source, "source", "identity")?;
        manifest_members(identity_value, "source identity")?;
        let identity = file_identity_from_json(source)
            .ok_or_else(|| ManifestError::schema("source identity is malformed".to_owned()))?;
        if identity.size != source_size {
            return Err(ManifestError::schema(format!(
                "source identity size {} does not match source size {source_size}",
                identity.size
            )));
        }

        let framing_value = manifest_field(&document, CONTEXT, "framing")?;
        manifest_members(framing_value, "framing")?;
        let framing = framing_descriptor_from_json(framing_value)
            .ok_or_else(|| ManifestError::schema("framing descriptor is malformed".to_owned()))?;
        if let FramingDescriptor::Delimiter { delimiter } = &framing {
            if let Ok(len_value) = manifest_u64(framing_value, "framing", "delimiter_len") {
                let len_usize = usize::try_from(len_value).unwrap_or(usize::MAX);
                if len_usize != delimiter.len() {
                    return Err(ManifestError::schema(format!(
                        "framing delimiter_len {len_value} does not match decoded delimiter length {}",
                        delimiter.len()
                    )));
                }
            }
        }
        if let Err(error) = framing.try_as_builtin() {
            return Err(ManifestError::schema(format!(
                "framing descriptor is not usable: {error}"
            )));
        }

        let partitioning = manifest_field(&document, CONTEXT, "partitioning")?;
        manifest_members(partitioning, "partitioning")?;
        let requested_partitions =
            manifest_usize(partitioning, "partitioning", "requested_partitions")?;
        let source_mode =
            parse_source_mode(manifest_field(partitioning, "partitioning", "source_mode")?)?;
        let window_bytes = match manifest_field(partitioning, "partitioning", "window_bytes")? {
            crate::json::Json::Null => None,
            value => Some(
                value
                    .as_u64()
                    .and_then(|raw| usize::try_from(raw).ok())
                    .ok_or_else(|| {
                        ManifestError::schema(
                            "partitioning field `window_bytes` must be an unsigned integer or null"
                                .to_owned(),
                        )
                    })?,
            ),
        };
        let index = parse_index_reference(manifest_field(
            partitioning,
            "partitioning",
            "source_index",
        )?)?;

        let ranges_value = manifest_field(&document, CONTEXT, "ranges")?;
        let entries = match ranges_value {
            crate::json::Json::Array(entries) => entries,
            _ => {
                return Err(ManifestError::schema(
                    "manifest field `ranges` must be a JSON array".to_owned(),
                ))
            }
        };
        if entries.len() > MAX_MANIFEST_RANGES {
            return Err(ManifestError::new(
                ManifestErrorKind::TooManyRanges,
                format!(
                    "manifest has {} ranges, limit is {MAX_MANIFEST_RANGES}",
                    entries.len()
                ),
            ));
        }
        let mut ranges = Vec::with_capacity(entries.len());
        for (position, entry) in entries.iter().enumerate() {
            let context = "range entry";
            manifest_members(entry, context)?;
            let index = manifest_usize(entry, context, "index")?;
            if index != position {
                return Err(ManifestError::schema(format!(
                    "range entry {position} carries index {index}; manifest order is authoritative"
                )));
            }
            let start = manifest_usize(entry, context, "start")?;
            let end = manifest_usize(entry, context, "end")?;
            let length = manifest_usize(entry, context, "length")?;
            let Some(span) = end.checked_sub(start) else {
                return Err(ManifestError::schema(format!(
                    "range entry {position} is inverted: start {start} >= end {end}"
                )));
            };
            if span == 0 {
                return Err(ManifestError::schema(format!(
                    "range entry {position} is empty; ranges must cover bytes"
                )));
            }
            if length != span {
                return Err(ManifestError::schema(format!(
                    "range entry {position} declares length {length} but spans {span} bytes"
                )));
            }
            let Ok(end_u64) = u64::try_from(end) else {
                return Err(ManifestError::schema(format!(
                    "range entry {position} end {end} does not fit in u64"
                )));
            };
            if end_u64 > source_size {
                return Err(ManifestError::schema(format!(
                    "range entry {position} ends at {end}, beyond source size {source_size}"
                )));
            }
            ranges.push((start, end));
        }

        Ok(Self {
            source_path,
            source_size,
            identity,
            framing,
            requested_partitions,
            source_mode,
            window_bytes,
            index,
            ranges,
        })
    }
}

/// Machine-readable reason a [`RangePlan`] coverage check failed.
///
/// Every variant carries the offending range index (when the failure is
/// attributable to one range) plus the expected and actual byte offsets
/// in [`CoverageError`], so workers can reject a manifest without
/// parsing human-readable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageKind {
    /// No ranges while `source_size > 0`.
    EmptyPlan,
    /// A range has `start >= end`.
    InvertedRange,
    /// The first range does not start at 0.
    BadStart,
    /// The last range does not end at `source_size`.
    BadEnd,
    /// The supplied bytes do not match `source_size`.
    SizeMismatch,
    /// A range starts before the previous range started.
    Unsorted,
    /// A range starts before the previous range ends.
    Overlap,
    /// A range starts after the previous range ends.
    Gap,
    /// A non-final range end is not a record boundary.
    Misaligned,
    /// Boundary replay itself failed (for example a truncated
    /// length-prefixed record); the plan cannot be proven aligned.
    Boundary,
    /// The framing descriptor is not a usable built-in strategy (for
    /// example an empty delimiter), so alignment cannot be replayed.
    InvalidFraming,
}

impl CoverageKind {
    /// Stable lowercase name for logs and wire diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmptyPlan => "empty_plan",
            Self::InvertedRange => "inverted_range",
            Self::BadStart => "bad_start",
            Self::BadEnd => "bad_end",
            Self::SizeMismatch => "size_mismatch",
            Self::Unsorted => "unsorted",
            Self::Gap => "gap",
            Self::Overlap => "overlap",
            Self::Misaligned => "misaligned",
            Self::Boundary => "replay_failed",
            Self::InvalidFraming => "invalid_framing",
        }
    }
}

/// Fail-closed coverage verdict for [`RangePlan::verify_coverage`].
///
/// `expected`/`actual` hold byte offsets in the units of the failing
/// check (see each [`CoverageKind`] variant); `detail` is a
/// human-readable explanation that never replaces the machine-readable
/// `kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageError {
    /// Machine-readable failure reason.
    pub kind: CoverageKind,
    /// Index of the offending range, when attributable to one range.
    pub index: Option<usize>,
    /// Expected byte offset for the failing check.
    pub expected: Option<usize>,
    /// Observed byte offset for the failing check.
    pub actual: Option<usize>,
    /// Human-readable explanation.
    pub detail: String,
}

impl CoverageError {
    fn new(
        kind: CoverageKind,
        index: Option<usize>,
        expected: Option<usize>,
        actual: Option<usize>,
        detail: String,
    ) -> Self {
        Self {
            kind,
            index,
            expected,
            actual,
            detail,
        }
    }
}

impl fmt::Display for CoverageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "coverage {}: {}",
            self.kind.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for CoverageError {}

/// In-memory [`ByteSource`] for coverage replay: no I/O, no mapping,
/// just the bytes the caller already holds.
struct SliceSource<'a> {
    data: &'a [u8],
}

impl ByteSource for SliceSource<'_> {
    fn len(&self) -> usize {
        self.data.len()
    }

    fn read_at(&self, offset: usize, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || offset >= self.data.len() {
            return Ok(0);
        }
        let count = out.len().min(self.data.len() - offset);
        out[..count].copy_from_slice(&self.data[offset..offset + count]);
        Ok(count)
    }

    fn as_slice(&self) -> Option<&[u8]> {
        Some(self.data)
    }
}

impl RangePlan {
    /// Canonical byte encoding of the plan manifest.
    ///
    /// This is the byte form of the already-serialized
    /// [`to_json`](Self::to_json) structure: key order, formatting, and
    /// range order are fixed, so equal plans always produce
    /// byte-identical output. Suitable as a stable pre-hash when
    /// workers digest manifests (hashing itself is the caller's job;
    /// this crate has no crypto and no hash dependencies).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.to_json().into_bytes()
    }

    /// Verify gap/overlap-free coverage plus record alignment, fail-closed.
    ///
    /// Checks against the supplied `data` (which must be the exact
    /// `source_size` bytes the plan was built for):
    ///
    /// - `data.len()` matches `source_size` ([`CoverageKind::SizeMismatch`]);
    /// - ranges are non-empty unless `source_size == 0`
    ///   ([`CoverageKind::EmptyPlan`]), each with `start < end`
    ///   ([`CoverageKind::InvertedRange`]);
    /// - the first range starts at 0 ([`CoverageKind::BadStart`]) and the
    ///   last range ends at `source_size` ([`CoverageKind::BadEnd`]);
    /// - ranges are sorted by start and exactly contiguous: no gaps
    ///   ([`CoverageKind::Gap`]), no overlaps ([`CoverageKind::Overlap`]);
    /// - every non-final range end is a record boundary, replayed through
    ///   the [`BuiltinFraming`] [`BoundaryScanner`] when the framing
    ///   descriptor maps to a built-in strategy
    ///   ([`CoverageKind::Misaligned`]). [`FramingDescriptor::Custom`]
    ///   plans skip the alignment replay (no scanner is available) but
    ///   still receive the full structural proof.
    ///
    /// Pure Rust helper: no I/O, no FFI, and no panics on untrusted
    /// input — every violation returns a machine-readable
    /// [`CoverageError`]. Scanner I/O failures during replay (for
    /// example truncated length-prefixed records) are reported as
    /// [`CoverageKind::Boundary`], never silently accepted.
    ///
    /// Note: this proves *coverage of the supplied bytes*, not source
    /// identity. Combine with [`identify_file`] (size/mtime plus the
    /// head/tail sample fingerprint) when the bytes come from anywhere
    /// but the planned file. The replay walks every record boundary —
    /// unlike the head/tail fingerprint, a middle-of-file edit that
    /// shifts boundaries cannot pass alignment — but in-record edits
    /// that preserve size and boundaries remain identity's job, not
    /// coverage's.
    pub fn verify_coverage(&self, data: &[u8]) -> Result<(), CoverageError> {
        let data_len = u64::try_from(data.len()).map_err(|_| {
            CoverageError::new(
                CoverageKind::SizeMismatch,
                None,
                usize::try_from(self.source_size).ok(),
                None,
                "supplied bytes do not fit in u64".to_owned(),
            )
        })?;
        if data_len != self.source_size {
            return Err(CoverageError::new(
                CoverageKind::SizeMismatch,
                None,
                usize::try_from(self.source_size).ok(),
                Some(data.len()),
                format!(
                    "plan covers {} bytes but {} bytes were supplied",
                    self.source_size,
                    data.len()
                ),
            ));
        }
        let size = usize::try_from(self.source_size).map_err(|_| {
            CoverageError::new(
                CoverageKind::SizeMismatch,
                None,
                None,
                Some(data.len()),
                format!(
                    "plan source size {} does not fit in usize",
                    self.source_size
                ),
            )
        })?;

        if self.ranges.is_empty() {
            if size == 0 {
                return Ok(());
            }
            return Err(CoverageError::new(
                CoverageKind::EmptyPlan,
                None,
                Some(size),
                Some(0),
                format!("plan has no ranges for {size} bytes"),
            ));
        }

        let mut previous_start = 0usize;
        let mut previous_end = 0usize;
        for (index, range) in self.ranges.iter().enumerate() {
            let (start, end) = *range;
            if start >= end {
                return Err(CoverageError::new(
                    CoverageKind::InvertedRange,
                    Some(index),
                    Some(start),
                    Some(end),
                    format!("range {index} is inverted: start {start} >= end {end}"),
                ));
            }
            if index == 0 {
                if start != 0 {
                    return Err(CoverageError::new(
                        CoverageKind::BadStart,
                        Some(0),
                        Some(0),
                        Some(start),
                        format!("first range starts at {start}, expected 0"),
                    ));
                }
            } else {
                if start < previous_start {
                    return Err(CoverageError::new(
                        CoverageKind::Unsorted,
                        Some(index),
                        Some(previous_start),
                        Some(start),
                        format!(
                            "range {index} starts at {start}, before previous start {previous_start}"
                        ),
                    ));
                }
                if start < previous_end {
                    return Err(CoverageError::new(
                        CoverageKind::Overlap,
                        Some(index),
                        Some(previous_end),
                        Some(start),
                        format!(
                            "range {index} starts at {start}, before previous end {previous_end}"
                        ),
                    ));
                }
                if start > previous_end {
                    return Err(CoverageError::new(
                        CoverageKind::Gap,
                        Some(index),
                        Some(previous_end),
                        Some(start),
                        format!(
                            "range {index} starts at {start}, after previous end {previous_end}"
                        ),
                    ));
                }
            }
            previous_start = start;
            previous_end = end;
        }
        if previous_end != size {
            let last = self.ranges.len() - 1;
            return Err(CoverageError::new(
                CoverageKind::BadEnd,
                Some(last),
                Some(size),
                Some(previous_end),
                format!("last range ends at {previous_end}, expected size {size}"),
            ));
        }

        let strategy = self.framing.try_as_builtin().map_err(|error| {
            CoverageError::new(
                CoverageKind::InvalidFraming,
                None,
                None,
                None,
                format!("framing descriptor is not a usable builtin: {error}"),
            )
        })?;
        let Some(strategy) = strategy else {
            return Ok(());
        };

        // The replay buffer only needs to satisfy the scanner's minimum;
        // bounding it by the file size keeps hand-built plans with huge
        // delimiters from forcing huge allocations (a pattern longer
        // than the file cannot match, so replay stays sound).
        let minimum = strategy.minimum_buffer_bytes().max(1);
        let buffer_len = minimum.min(data.len().max(1));
        let mut buffer = vec![0u8; buffer_len];
        let source = SliceSource { data };
        let mut scanner = strategy.scanner(&source, &mut buffer);

        // Non-final ends are strictly increasing and each must coincide
        // with a record boundary; both sequences are sorted, so a single
        // streaming pass needs O(1) extra memory.
        let last_index = self.ranges.len() - 1;
        let mut pending = self.ranges[..last_index]
            .iter()
            .enumerate()
            .map(|(index, range)| (index, range.1));
        let mut current = pending.next();
        let mut floor = 0usize;
        while let Some((wanted_index, wanted_end)) = current {
            let boundary = scanner.boundary_after(floor).map_err(|error| {
                CoverageError::new(
                    CoverageKind::Boundary,
                    Some(wanted_index),
                    Some(wanted_end),
                    None,
                    format!("boundary replay failed before range end {wanted_end}: {error}"),
                )
            })?;
            let Some(boundary) = boundary else {
                return Err(CoverageError::new(
                    CoverageKind::Misaligned,
                    Some(wanted_index),
                    Some(wanted_end),
                    None,
                    format!(
                        "range {wanted_index} end {wanted_end} is past the last record boundary"
                    ),
                ));
            };
            if boundary <= floor {
                return Err(CoverageError::new(
                    CoverageKind::Boundary,
                    Some(wanted_index),
                    Some(wanted_end),
                    Some(boundary),
                    format!("boundary scanner did not advance at {boundary}"),
                ));
            }
            floor = boundary;
            if boundary > wanted_end {
                return Err(CoverageError::new(
                    CoverageKind::Misaligned,
                    Some(wanted_index),
                    Some(wanted_end),
                    Some(boundary),
                    format!(
                        "range {wanted_index} end {wanted_end} is not a record boundary (next is {boundary})"
                    ),
                ));
            }
            if boundary == wanted_end {
                current = pending.next();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner;

    fn temp_file(label: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mmap_chunker_manifest_{label}"));
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

    #[test]
    fn identify_file_reports_size_and_fingerprint() {
        let content = b"alpha\r\nbeta\r\ngamma\r\n";
        let path = temp_file("identify", content);
        let identity = identify_file(&path).unwrap();
        assert_eq!(identity.size, content.len() as u64);
        assert!(identity.modified_unix_nanos.is_some());
        let fingerprint = identity.sample_fingerprint.unwrap();
        assert!(fingerprint.starts_with(FNV1A64_PREFIX));
        assert_eq!(identity.sample_bytes, IDENTITY_SAMPLE_BYTES);
        cleanup(&path);
    }

    #[test]
    fn fingerprint_is_deterministic_and_content_sensitive() {
        let first = temp_file("fingerprint_a", b"same size content!!!");
        let second = temp_file("fingerprint_b", b"same size content???");
        let third = temp_file("fingerprint_c", b"same size content!!!");

        let first_identity = identify_file(&first).unwrap();
        let second_identity = identify_file(&second).unwrap();
        let third_identity = identify_file(&third).unwrap();

        assert_eq!(
            first_identity.sample_fingerprint,
            third_identity.sample_fingerprint
        );
        assert_ne!(
            first_identity.sample_fingerprint,
            second_identity.sample_fingerprint
        );

        cleanup(&first);
        cleanup(&second);
        cleanup(&third);
    }

    #[test]
    fn fingerprint_handles_files_smaller_than_two_samples() {
        let path = temp_file("small", b"tiny");
        let identity = identify_file(&path).unwrap();
        assert!(identity.sample_fingerprint.is_some());
        cleanup(&path);
    }

    #[test]
    fn fingerprint_reads_head_and_tail_around_the_sample_limit() {
        let mut content = vec![b'a'; IDENTITY_SAMPLE_BYTES * 3];
        content[0] = b'X';
        content[IDENTITY_SAMPLE_BYTES * 3 - 1] = b'Y';
        let path = temp_file("large", &content);

        let identity = identify_file(&path).unwrap();
        let baseline = identity.sample_fingerprint.clone().unwrap();

        // Change a byte in the middle: outside both samples, so the
        // fingerprint intentionally stays the same.
        content[IDENTITY_SAMPLE_BYTES + IDENTITY_SAMPLE_BYTES / 2] = b'Z';
        std::fs::write(&path, &content).unwrap();
        let middle_changed = identify_file(&path).unwrap();
        assert_eq!(baseline, middle_changed.sample_fingerprint.unwrap());

        // Change the first byte: inside the head sample.
        content[0] = b'Q';
        std::fs::write(&path, &content).unwrap();
        let head_changed = identify_file(&path).unwrap();
        assert_ne!(baseline, head_changed.sample_fingerprint.unwrap());

        cleanup(&path);
    }

    #[test]
    fn plan_file_matches_slice_scanner_and_seals_identity() {
        let mut content = Vec::new();
        for index in 0..400u32 {
            content.extend_from_slice(format!("record-{index:04}\r\n").as_bytes());
        }
        let path = temp_file("plan", &content);
        let expected = scanner::find_partition_boundaries_pattern(&content, 5, b"\r\n");

        let plan = unsafe { plan_file(&path, 5, b"\r\n", SourceMode::Mmap, 0) }.unwrap();
        assert_eq!(plan.ranges, expected);
        assert_eq!(plan.source_size, content.len() as u64);
        assert_eq!(plan.requested_partitions, 5);
        assert_eq!(plan.source_mode, Some(SourceMode::Mmap));
        assert_eq!(plan.window_bytes, None);
        assert_eq!(
            plan.framing,
            FramingDescriptor::Delimiter {
                delimiter: b"\r\n".to_vec()
            }
        );
        assert_eq!(plan.identity, identify_file(&path).unwrap());
        assert_eq!(plan.index, None);

        cleanup(&path);
    }

    #[test]
    fn plan_file_windowed_records_window_bytes() {
        let content = b"a\r\nb\r\nc\r\nd\r\n";
        let path = temp_file("plan_windowed", content);
        let window = 65536usize;
        let plan = unsafe { plan_file(&path, 2, b"\r\n", SourceMode::Windowed, window) }.unwrap();
        assert_eq!(plan.window_bytes, Some(window));
        assert_eq!(plan.ranges, vec![(0, 9), (9, 12)]);
        cleanup(&path);
    }

    #[test]
    fn plan_json_contains_schema_identity_framing_and_ranges() {
        let path = temp_file("plan_json", b"one\r\ntwo\r\nthree\r\n");
        let plan = unsafe { plan_file(&path, 2, b"\r\n", SourceMode::Mmap, 0) }.unwrap();
        let json = plan.to_json();

        assert!(json.contains("\"schema\": \"mmap-chunker-plan\""));
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"name\": \"mmap-chunker-core\""));
        assert!(json.contains("\"delimiter_hex\": \"0d0a\""));
        assert!(json.contains("\"delimiter_len\": 2"));
        assert!(json.contains("\"strategy\": \"bytes\""));
        assert!(json.contains("\"source_mode\": \"mmap\""));
        assert!(json.contains("\"window_bytes\": null"));
        assert!(json.contains("\"requested_partitions\": 2"));
        assert!(json.contains("\"actual_partitions\": 2"));
        assert!(json.contains(&format!("\"size\": {}", "one\r\ntwo\r\nthree\r\n".len())));
        assert!(json.contains("\"sample_fingerprint\": \"fnv1a64:0x"));
        assert!(json.contains("\"index\": 0"));
        assert!(json.contains("\"index\": 1"));
        assert!(json.contains("\"length\": 10"));
        assert!(json.contains("\"length\": 7"));

        // Deterministic: same plan -> byte-identical JSON.
        assert_eq!(json, plan.to_json());
        cleanup(&path);
    }

    #[test]
    fn json_escapes_special_characters_in_paths() {
        let mut identity = FileIdentity {
            size: 0,
            modified_unix_nanos: None,
            device: None,
            inode: None,
            sample_fingerprint: None,
            sample_bytes: IDENTITY_SAMPLE_BYTES,
        };
        identity.sample_fingerprint = Some("fnv1a64:0x0000000000000000".to_owned());
        let plan = RangePlan {
            source_path: "C:\\data\\\"quoted\"\nline.jsonl".to_owned(),
            source_size: 0,
            identity,
            framing: FramingDescriptor::Delimiter {
                delimiter: b"\n".to_vec(),
            },
            requested_partitions: 1,
            source_mode: Some(SourceMode::Pread),
            window_bytes: None,
            index: None,
            ranges: Vec::new(),
        };

        let json = plan.to_json();
        assert!(json.contains("C:\\\\data\\\\\\\"quoted\\\"\\nline.jsonl"));
        assert!(json.contains("\"ranges\": []"));
        assert!(json.contains("\"source_mode\": \"pread\""));
    }

    #[test]
    fn plan_file_rejects_empty_delimiter() {
        let path = temp_file("empty_delim", b"data");
        let error = unsafe { plan_file(&path, 2, b"", SourceMode::Mmap, 0) }.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        cleanup(&path);
    }

    #[test]
    fn plan_file_fixed_width_framing() {
        let content: Vec<u8> = (0..100u8).collect();
        let path = temp_file("fixed_width", &content);
        let strategy = BuiltinFraming::fixed_width(16).unwrap();
        let plan =
            unsafe { plan_file_with_framing(&path, 4, &strategy, SourceMode::Mmap, 0) }.unwrap();

        assert_eq!(
            plan.framing,
            FramingDescriptor::FixedWidth { record_bytes: 16 }
        );
        assert_eq!(plan.ranges, vec![(0, 32), (32, 64), (64, 80), (80, 100)]);
        let json = plan.to_json();
        assert!(json.contains("\"strategy\": \"fixed_width\""));
        assert!(json.contains("\"record_bytes\": 16"));
        cleanup(&path);
    }

    #[test]
    fn plan_file_length_prefixed_framing() {
        let payloads: &[&[u8]] = &[b"alpha", b"be", b"gamma-payload", b""];
        let mut content = Vec::new();
        for payload in payloads {
            content.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            content.extend_from_slice(payload);
        }
        let path = temp_file("length_prefixed", &content);
        let strategy = BuiltinFraming::length_prefixed(2, true, false).unwrap();
        let plan =
            unsafe { plan_file_with_framing(&path, 2, &strategy, SourceMode::Pread, 0) }.unwrap();

        assert_eq!(
            plan.framing,
            FramingDescriptor::LengthPrefixed {
                prefix_bytes: 2,
                little_endian: true,
                length_includes_prefix: false,
            }
        );
        assert_eq!(plan.ranges, vec![(0, 26), (26, 28)]);
        assert_eq!(plan.source_mode, Some(SourceMode::Pread));

        let json = plan.to_json();
        assert!(json.contains("\"strategy\": \"length_prefixed\""));
        assert!(json.contains("\"prefix_bytes\": 2"));
        assert!(json.contains("\"little_endian\": true"));
        assert!(json.contains("\"length_includes_prefix\": false"));
        cleanup(&path);
    }

    #[test]
    fn plan_from_ranges_seals_identity_and_records_index_reference() {
        let content: Vec<u8> = (0..64u8).collect();
        let path = temp_file("plan_from_ranges", &content);
        let identity = identify_file(&path).unwrap();
        let plan = plan_from_ranges(
            &path,
            &identity,
            FramingDescriptor::FixedWidth { record_bytes: 16 },
            2,
            IndexReference {
                stride: 4,
                record_count: 4,
                path: Some("data.bin.mmapidx".to_owned()),
            },
            vec![(0, 32), (32, 64)],
        )
        .unwrap();

        assert_eq!(plan.source_mode, None);
        assert_eq!(plan.index.as_ref().unwrap().stride, 4);
        let json = plan.to_json();
        assert!(json.contains("\"strategy\": \"indexed_records\""));
        assert!(json.contains("\"source_mode\": null"));
        assert!(json.contains("\"record_count\": 4"));
        assert!(json.contains("\"path\": \"data.bin.mmapidx\""));

        let mut tampered = identity.clone();
        tampered.size += 1;
        assert!(plan_from_ranges(
            &path,
            &tampered,
            FramingDescriptor::FixedWidth { record_bytes: 16 },
            2,
            IndexReference {
                stride: 4,
                record_count: 4,
                path: None,
            },
            vec![(0, 64)],
        )
        .is_err());

        cleanup(&path);
    }

    #[test]
    fn canonical_bytes_are_deterministic_for_equal_plans() {
        let plan = RangePlan {
            source_path: "data.bin".to_owned(),
            source_size: 8,
            identity: FileIdentity {
                size: 8,
                modified_unix_nanos: None,
                device: None,
                inode: None,
                sample_fingerprint: Some("fnv1a64:0x0000000000000000".to_owned()),
                sample_bytes: IDENTITY_SAMPLE_BYTES,
            },
            framing: FramingDescriptor::Delimiter {
                delimiter: b"\n".to_vec(),
            },
            requested_partitions: 2,
            source_mode: Some(SourceMode::Pread),
            window_bytes: None,
            index: None,
            ranges: vec![(0, 4), (4, 8)],
        };

        assert_eq!(plan.canonical_bytes(), plan.to_json().into_bytes());
        assert_eq!(
            plan.canonical_bytes(),
            plan.clone().canonical_bytes(),
            "equal plans must canonicalize to identical bytes"
        );
    }
}
