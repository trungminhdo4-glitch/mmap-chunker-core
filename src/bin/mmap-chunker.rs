use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mmap_chunker_core::{
    build_record_index, identify_file, plan_file_with_framing, plan_from_ranges,
    plan_partition_boundaries_from_index, plan_partition_ranges_with, BuiltinFraming,
    IndexReference, MmapFile, PlannerOptions, RangePlan, RecordIndex, SourceMode,
    MAX_LENGTH_PREFIX_BYTES, MAX_MANIFEST_BYTES, MIN_WINDOW_BYTES, PLAN_SCHEMA,
    PLAN_SCHEMA_VERSION,
};

const HELP: &str = "\
mmap-chunker - record-aligned byte-range planning for immutable local files

Usage:
  mmap-chunker partition FILE --parts N [framing options] [--source MODE] [--window BYTES] [--worker K]
  mmap-chunker plan FILE --parts N [framing options] [--source MODE] [--window BYTES] [--output PATH]
  mmap-chunker plan FILE --parts N --index PATH [--output PATH]
  mmap-chunker index FILE --every N [framing options] [--source MODE] [--window BYTES] [--output PATH]
  mmap-chunker verify MANIFEST FILE
  mmap-chunker --help
  mmap-chunker --version

Commands:
  partition    Emit record-aligned byte ranges for FILE as numeric TSV.
  plan         Write a reproducible, versioned range-plan manifest (JSON).
               The manifest seals the source identity (size, mtime,
               device/inode when available, sampled content fingerprint) so a
               stale plan can be rejected before workers consume it. With
               --index, ranges are derived from a sparse record index instead
               of rescanning the file.
  index        Build a sparse record index sidecar for FILE. Every Nth record
               start is recorded together with the file identity and framing,
               so repeated planning does not rescan the source. Default output
               is FILE.mmapidx.
  verify       Check a stored range-plan manifest against FILE: schema,
               coverage, and record boundaries. Prints `verify ok` and exits
               0 only when the ranges cover FILE gap- and overlap-free on
               record boundaries; any violation fails with a stable
               machine-readable kind token. This proves coverage of the
               presented bytes, not full content integrity (see the
               `verify ok` line for exactly what was checked).

Framing options:
  --delimiter-byte B
                Record delimiter byte in decimal (0..255). Defaults to 10
                (LF/newline). Raw byte framing only; no CSV/JSON quoting
                semantics. Mutually exclusive with --delimiter-hex.
  --delimiter-hex HEX
                Record delimiter bytes as an even-length hex string, e.g.
                0d0a for CRLF or 0d0a0d0a for blank-line-separated records.
  --framing MODE
                Record framing: delimiter (default), fixed, or length-prefixed.
  --record-bytes N
                Fixed-width record size in bytes (required for --framing fixed).
  --prefix-bytes N
                Length-prefix width in bytes, 1..=8 (required for
                --framing length-prefixed).
  --prefix-endian ENDIAN
                Length-prefix byte order: le (default) or be.
  --length-includes-prefix
                Treat the prefix value as the total record size including the
                prefix (default: the value counts payload bytes only).

Other options:
  --parts N     Request N record-aligned partitions.
  --source MODE Byte source backend: mmap (default), windowed (bounded
                moving-window mapping), or pread (positional reads, no
                mapping). All modes produce identical ranges.
  --window BYTES
                Window size in bytes for --source windowed (default 67108864,
                minimum 65536). Requires --source windowed.
  --every N     Record-index stride: record every Nth record start.
                Only valid for `index`.
  --index PATH  Read a sparse record index instead of scanning FILE.
                Only valid for `plan`; framing and source options are then
                taken from the index.
  --worker K    Emit only zero-based worker K's actual partition. Only valid
                for `partition`. If no actual partition K exists, the command
                succeeds silently.
  --output PATH Write the JSON artifact to PATH instead of stdout
                (both `plan` and `index`). `index` defaults to FILE.mmapidx.

Output:
  partition: index<TAB>start<TAB>end_exclusive<TAB>length
  plan:      JSON manifest (schema \"mmap-chunker-plan\", schema_version 1)
  index:     JSON sidecar (schema \"mmap-chunker-index\", schema_version 1)

Offsets are bytes; starts are inclusive and ends are exclusive. The input file
must remain immutable while it is planned. The actual number of ranges can be
lower than N when records span multiple ideal partition positions. Every
non-final range ends on a complete record boundary.\n";

fn main() -> ExitCode {
    match run(env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("Try `mmap-chunker --help` for usage.");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Partition,
    Plan,
    Index,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FramingChoice {
    Delimiter(Vec<u8>),
    FixedWidth(usize),
    LengthPrefixed {
        prefix_bytes: u8,
        little_endian: bool,
        length_includes_prefix: bool,
    },
}

impl FramingChoice {
    fn strategy(&self) -> BuiltinFraming {
        match self {
            Self::Delimiter(delimiter) => BuiltinFraming::Delimiter(delimiter.clone()),
            Self::FixedWidth { .. } => BuiltinFraming::FixedWidth {
                record_bytes: match self {
                    Self::FixedWidth(record_bytes) => *record_bytes,
                    _ => unreachable!("variant checked by match"),
                },
            },
            Self::LengthPrefixed {
                prefix_bytes,
                little_endian,
                length_includes_prefix,
            } => BuiltinFraming::LengthPrefixed {
                prefix_bytes: *prefix_bytes,
                little_endian: *little_endian,
                length_includes_prefix: *length_includes_prefix,
            },
        }
    }
}

#[derive(Debug)]
struct PlanningArguments {
    file: PathBuf,
    parts: Option<usize>,
    framing: FramingChoice,
    framing_explicit: bool,
    source: SourceMode,
    source_seen: bool,
    window: Option<usize>,
    worker: Option<usize>,
    output: Option<PathBuf>,
    index: Option<PathBuf>,
    every: Option<u64>,
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    let arguments: Vec<OsString> = arguments.into_iter().collect();
    match arguments.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => {
            print!("{HELP}");
            Ok(())
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("mmap-chunker {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [command, rest @ ..] if command == "partition" => {
            run_planning_command(rest, CommandKind::Partition)
        }
        [command, rest @ ..] if command == "plan" => run_planning_command(rest, CommandKind::Plan),
        [command, rest @ ..] if command == "index" => {
            run_planning_command(rest, CommandKind::Index)
        }
        [command, manifest, file] if command == "verify" => {
            run_verify(manifest.into(), file.into())
        }
        [command, ..] if command == "verify" => Err(
            "usage: mmap-chunker verify MANIFEST FILE (exactly two positional arguments)"
                .to_owned(),
        ),
        [] => Err("missing command".to_owned()),
        [command, ..] => Err(format!("unknown command `{}`", command.to_string_lossy())),
    }
}

fn run_planning_command(arguments: &[OsString], kind: CommandKind) -> Result<(), String> {
    if arguments.len() == 1 && (arguments[0] == "--help" || arguments[0] == "-h") {
        print!("{HELP}");
        return Ok(());
    }

    let parsed = parse_planning_arguments(arguments, kind)?;
    let options = match (parsed.source, parsed.window) {
        (SourceMode::Windowed, Some(window_bytes)) => {
            PlannerOptions::new(SourceMode::Windowed).with_window_bytes(window_bytes)
        }
        (SourceMode::Windowed, None) => PlannerOptions::new(SourceMode::Windowed),
        (_, Some(_)) => {
            return Err("`--window` requires `--source windowed`".to_owned());
        }
        (mode, None) => PlannerOptions::new(mode),
    };

    match kind {
        CommandKind::Partition => {
            let parts = parsed
                .parts
                .ok_or_else(|| "missing required option `--parts`".to_owned())?;
            if parsed.index.is_some() {
                return Err("`--index` is only valid for `plan`".to_owned());
            }
            if parsed.every.is_some() {
                return Err("`--every` is only valid for `index`".to_owned());
            }
            let strategy = parsed.framing.strategy();
            if let Some(worker) = parsed.worker {
                if worker >= parts {
                    return Err("`--worker` must be less than `--parts`".to_owned());
                }
                emit_partitions(parsed.file, parts, &strategy, Some(worker), &options)
            } else {
                emit_partitions(parsed.file, parts, &strategy, None, &options)
            }
        }
        CommandKind::Plan => {
            let parts = parsed
                .parts
                .ok_or_else(|| "missing required option `--parts`".to_owned())?;
            if parsed.every.is_some() {
                return Err("`--every` is only valid for `index`".to_owned());
            }
            match parsed.index {
                Some(index_path) => {
                    if parsed.framing_explicit {
                        return Err(
                            "framing options are not allowed with `--index`; the index defines the framing"
                                .to_owned(),
                        );
                    }
                    if parsed.source_seen || parsed.window.is_some() {
                        return Err(
                            "`--source`/`--window` are not allowed with `--index`; indexed planning does not scan the file"
                                .to_owned(),
                        );
                    }
                    emit_plan_from_index(parsed.file, index_path, parts, parsed.output)
                }
                None => {
                    let strategy = parsed.framing.strategy();
                    emit_plan(parsed.file, parts, &strategy, &options, parsed.output)
                }
            }
        }
        CommandKind::Index => {
            let every = parsed
                .every
                .ok_or_else(|| "missing required option `--every`".to_owned())?;
            if parsed.parts.is_some() {
                return Err("`--parts` is not valid for `index`".to_owned());
            }
            if parsed.worker.is_some() {
                return Err("`--worker` is not valid for `index`".to_owned());
            }
            if parsed.index.is_some() {
                return Err("`--index` is not valid for `index`".to_owned());
            }
            let strategy = parsed.framing.strategy();
            emit_index(parsed.file, &strategy, every, &options, parsed.output)
        }
    }
}

fn parse_planning_arguments(
    arguments: &[OsString],
    kind: CommandKind,
) -> Result<PlanningArguments, String> {
    let mut file = None;
    let mut parts = None;
    let mut delimiter: Option<Vec<u8>> = None;
    let mut delimiter_seen = false;
    let mut framing_name: Option<String> = None;
    let mut framing_seen = false;
    let mut record_bytes: Option<usize> = None;
    let mut prefix_bytes: Option<u8> = None;
    let mut prefix_endian: Option<bool> = None;
    let mut length_includes_prefix = false;
    let mut source = SourceMode::Mmap;
    let mut source_seen = false;
    let mut window: Option<usize> = None;
    let mut worker = None;
    let mut output = None;
    let mut index_path = None;
    let mut every = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--parts" {
            if parts.is_some() {
                return Err("duplicate option `--parts`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--parts`".to_owned())?;
            parts = Some(parse_parts(value)?);
        } else if argument == "--delimiter-byte" {
            if delimiter_seen {
                return Err(
                    "duplicate delimiter option (`--delimiter-byte` / `--delimiter-hex`)"
                        .to_owned(),
                );
            }
            delimiter_seen = true;
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--delimiter-byte`".to_owned())?;
            delimiter = Some(vec![parse_delimiter_byte(value)?]);
        } else if argument == "--delimiter-hex" {
            if delimiter_seen {
                return Err(
                    "duplicate delimiter option (`--delimiter-byte` / `--delimiter-hex`)"
                        .to_owned(),
                );
            }
            delimiter_seen = true;
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--delimiter-hex`".to_owned())?;
            delimiter = Some(parse_delimiter_hex(value)?);
        } else if argument == "--framing" {
            if framing_seen {
                return Err("duplicate option `--framing`".to_owned());
            }
            framing_seen = true;
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--framing`".to_owned())?;
            framing_name = Some(parse_framing_name(value)?);
        } else if argument == "--record-bytes" {
            if record_bytes.is_some() {
                return Err("duplicate option `--record-bytes`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--record-bytes`".to_owned())?;
            record_bytes = Some(parse_positive_usize(value, "--record-bytes")?);
        } else if argument == "--prefix-bytes" {
            if prefix_bytes.is_some() {
                return Err("duplicate option `--prefix-bytes`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--prefix-bytes`".to_owned())?;
            prefix_bytes = Some(parse_prefix_bytes(value)?);
        } else if argument == "--prefix-endian" {
            if prefix_endian.is_some() {
                return Err("duplicate option `--prefix-endian`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--prefix-endian`".to_owned())?;
            prefix_endian = Some(parse_prefix_endian(value)?);
        } else if argument == "--length-includes-prefix" {
            length_includes_prefix = true;
        } else if argument == "--source" {
            if source_seen {
                return Err("duplicate option `--source`".to_owned());
            }
            source_seen = true;
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--source`".to_owned())?;
            source = parse_source(value)?;
        } else if argument == "--window" {
            if window.is_some() {
                return Err("duplicate option `--window`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--window`".to_owned())?;
            window = Some(parse_window(value)?);
        } else if argument == "--every" {
            if every.is_some() {
                return Err("duplicate option `--every`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--every`".to_owned())?;
            every = Some(parse_every(value)?);
        } else if argument == "--index" {
            if index_path.is_some() {
                return Err("duplicate option `--index`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--index`".to_owned())?;
            index_path = Some(PathBuf::from(value));
        } else if argument == "--worker" {
            if worker.is_some() {
                return Err("duplicate option `--worker`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--worker`".to_owned())?;
            worker = Some(parse_worker(value)?);
        } else if argument == "--output" {
            if output.is_some() {
                return Err("duplicate option `--output`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--output`".to_owned())?;
            output = Some(PathBuf::from(value));
        } else if argument.as_os_str().to_string_lossy().starts_with('-') {
            return Err(format!(
                "unexpected option `{}`",
                argument.to_string_lossy()
            ));
        } else if file.replace(PathBuf::from(argument)).is_some() {
            return Err(format!(
                "unexpected argument `{}`",
                argument.to_string_lossy()
            ));
        }
        index += 1;
    }

    if kind == CommandKind::Partition && output.is_some() {
        return Err("`--output` is only valid for `plan` and `index`".to_owned());
    }
    if kind != CommandKind::Partition && worker.is_some() {
        return Err("`--worker` is only valid for `partition`".to_owned());
    }

    let file = file.ok_or_else(|| "missing FILE".to_owned())?;
    let framing_explicit = framing_name.is_some()
        || delimiter.is_some()
        || record_bytes.is_some()
        || prefix_bytes.is_some()
        || prefix_endian.is_some()
        || length_includes_prefix;
    let framing = build_framing(
        framing_name.as_deref(),
        delimiter,
        record_bytes,
        prefix_bytes,
        prefix_endian,
        length_includes_prefix,
    )?;

    Ok(PlanningArguments {
        file,
        parts,
        framing,
        framing_explicit,
        source,
        source_seen,
        window,
        worker,
        output,
        index: index_path,
        every,
    })
}

fn build_framing(
    framing_name: Option<&str>,
    delimiter: Option<Vec<u8>>,
    record_bytes: Option<usize>,
    prefix_bytes: Option<u8>,
    prefix_endian: Option<bool>,
    length_includes_prefix: bool,
) -> Result<FramingChoice, String> {
    match framing_name.unwrap_or("delimiter") {
        "delimiter" => {
            if record_bytes.is_some() {
                return Err("`--record-bytes` requires `--framing fixed`".to_owned());
            }
            if prefix_bytes.is_some() || prefix_endian.is_some() || length_includes_prefix {
                return Err(
                    "`--prefix-bytes`/`--prefix-endian`/`--length-includes-prefix` require \
                     `--framing length-prefixed`"
                        .to_owned(),
                );
            }
            Ok(FramingChoice::Delimiter(
                delimiter.unwrap_or_else(|| vec![0x0A]),
            ))
        }
        "fixed" => {
            if delimiter.is_some() {
                return Err("`--delimiter-byte`/`--delimiter-hex` are not valid with `--framing fixed`".to_owned());
            }
            if prefix_bytes.is_some() || prefix_endian.is_some() || length_includes_prefix {
                return Err(
                    "length-prefix options are not valid with `--framing fixed`".to_owned(),
                );
            }
            let record_bytes = record_bytes
                .ok_or_else(|| "`--framing fixed` requires `--record-bytes`".to_owned())?;
            Ok(FramingChoice::FixedWidth(record_bytes))
        }
        "length-prefixed" => {
            if delimiter.is_some() {
                return Err("`--delimiter-byte`/`--delimiter-hex` are not valid with `--framing length-prefixed`".to_owned());
            }
            if record_bytes.is_some() {
                return Err(
                    "`--record-bytes` is not valid with `--framing length-prefixed`".to_owned(),
                );
            }
            let prefix_bytes = prefix_bytes
                .ok_or_else(|| "`--framing length-prefixed` requires `--prefix-bytes`".to_owned())?;
            Ok(FramingChoice::LengthPrefixed {
                prefix_bytes,
                little_endian: prefix_endian.unwrap_or(true),
                length_includes_prefix,
            })
        }
        other => Err(format!(
            "invalid value for `--framing`: `{other}` (expected delimiter, fixed, or length-prefixed)"
        )),
    }
}

fn parse_framing_name(value: &OsStr) -> Result<String, String> {
    match value.to_str() {
        Some(name @ ("delimiter" | "fixed" | "length-prefixed")) => Ok(name.to_owned()),
        _ => Err(format!(
            "invalid value for `--framing`: `{}` (expected delimiter, fixed, or length-prefixed)",
            value.to_string_lossy()
        )),
    }
}

fn parse_positive_usize(value: &OsStr, option: &str) -> Result<usize, String> {
    let text = value
        .to_str()
        .ok_or_else(|| format!("`{option}` must be a positive integer"))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "invalid value for `{option}`: `{}` (expected a positive integer)",
            value.to_string_lossy()
        ));
    }
    let parsed = text.parse::<usize>().map_err(|_| {
        format!(
            "invalid value for `{option}`: `{}` (expected a positive integer)",
            value.to_string_lossy()
        )
    })?;
    if parsed == 0 {
        return Err(format!("`{option}` must be greater than zero"));
    }
    Ok(parsed)
}

fn parse_prefix_bytes(value: &OsStr) -> Result<u8, String> {
    let parsed = parse_positive_usize(value, "--prefix-bytes")?;
    if parsed > usize::from(MAX_LENGTH_PREFIX_BYTES) {
        return Err(format!(
            "`--prefix-bytes` must be between 1 and {MAX_LENGTH_PREFIX_BYTES}, got {parsed}"
        ));
    }
    u8::try_from(parsed).map_err(|_| "`--prefix-bytes` must fit in a byte".to_owned())
}

fn parse_prefix_endian(value: &OsStr) -> Result<bool, String> {
    match value.to_str() {
        Some("le") => Ok(true),
        Some("be") => Ok(false),
        _ => Err(format!(
            "invalid value for `--prefix-endian`: `{}` (expected le or be)",
            value.to_string_lossy()
        )),
    }
}

fn parse_every(value: &OsStr) -> Result<u64, String> {
    let parsed = parse_positive_usize(value, "--every")?;
    u64::try_from(parsed).map_err(|_| "`--every` exceeds the supported range".to_owned())
}

fn parse_parts(value: &OsStr) -> Result<usize, String> {
    let parsed = value
        .to_str()
        .ok_or_else(|| "`--parts` must be a positive integer".to_owned())?
        .parse::<usize>()
        .map_err(|_| format!("invalid value for `--parts`: `{}`", value.to_string_lossy()))?;
    if parsed == 0 {
        return Err("`--parts` must be greater than zero".to_owned());
    }
    Ok(parsed)
}

fn parse_worker(value: &OsStr) -> Result<usize, String> {
    value
        .to_str()
        .ok_or_else(|| "`--worker` must be a non-negative integer".to_owned())?
        .parse::<usize>()
        .map_err(|_| {
            format!(
                "invalid value for `--worker`: `{}`",
                value.to_string_lossy()
            )
        })
}

fn parse_delimiter_byte(value: &OsStr) -> Result<u8, String> {
    let value_text = value.to_str().ok_or_else(|| {
        "`--delimiter-byte` must be a decimal byte in the range 0..255".to_owned()
    })?;
    if value_text.is_empty() || !value_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "invalid value for `--delimiter-byte`: `{}` (expected decimal byte 0..255)",
            value.to_string_lossy()
        ));
    }
    let parsed = value_text.parse::<u16>().map_err(|_| {
        format!(
            "invalid value for `--delimiter-byte`: `{}` (expected decimal byte 0..255)",
            value.to_string_lossy()
        )
    })?;
    u8::try_from(parsed).map_err(|_| {
        format!(
            "invalid value for `--delimiter-byte`: `{}` (expected decimal byte 0..255)",
            value.to_string_lossy()
        )
    })
}

fn decode_hex_pair(pair: &[u8]) -> u8 {
    let text = std::str::from_utf8(pair).expect("validated hex pair must be ASCII");
    u8::from_str_radix(text, 16).expect("validated hex pair must parse")
}

fn parse_delimiter_hex(value: &OsStr) -> Result<Vec<u8>, String> {
    let invalid = || {
        format!(
            "invalid value for `--delimiter-hex`: `{}` (expected an even-length hex string, e.g. 0d0a)",
            value.to_string_lossy()
        )
    };
    let text = value.to_str().ok_or_else(invalid)?;
    if text.is_empty() || text.len() % 2 != 0 || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid());
    }
    Ok(text
        .as_bytes()
        .chunks_exact(2)
        .map(decode_hex_pair)
        .collect())
}

fn parse_source(value: &OsStr) -> Result<SourceMode, String> {
    match value.to_str() {
        Some("mmap") => Ok(SourceMode::Mmap),
        Some("windowed") => Ok(SourceMode::Windowed),
        Some("pread") => Ok(SourceMode::Pread),
        _ => Err(format!(
            "invalid value for `--source`: `{}` (expected mmap, windowed, or pread)",
            value.to_string_lossy()
        )),
    }
}

fn parse_window(value: &OsStr) -> Result<usize, String> {
    let text = value
        .to_str()
        .ok_or_else(|| "`--window` must be a positive integer".to_owned())?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "invalid value for `--window`: `{}` (expected bytes, minimum {MIN_WINDOW_BYTES})",
            value.to_string_lossy()
        ));
    }
    let parsed = text.parse::<usize>().map_err(|_| {
        format!(
            "invalid value for `--window`: `{}` (expected bytes, minimum {MIN_WINDOW_BYTES})",
            value.to_string_lossy()
        )
    })?;
    if parsed < MIN_WINDOW_BYTES {
        return Err(format!(
            "`--window` must be at least {MIN_WINDOW_BYTES} bytes, got {parsed}"
        ));
    }
    Ok(parsed)
}

fn emit_partitions(
    path: PathBuf,
    parts: usize,
    strategy: &BuiltinFraming,
    worker: Option<usize>,
    options: &PlannerOptions,
) -> Result<(), String> {
    // Safety: the CLI's contract requires the input file to remain immutable while planned.
    let ranges = unsafe { plan_partition_ranges_with(&path, parts, strategy, options) }
        .map_err(|error| format!("failed to plan ranges for {}: {error}", path.display()))?;

    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());

    let indices = match worker {
        Some(index) if index < ranges.len() => index..index + 1,
        Some(_) => 0..0,
        None => 0..ranges.len(),
    };
    for index in indices {
        let (start, end) = ranges[index];
        let length = end - start;
        writeln!(output, "{index}\t{start}\t{end}\t{length}")
            .map_err(|error| format!("failed to write output: {error}"))?;
    }
    output
        .flush()
        .map_err(|error| format!("failed to write output: {error}"))
}

fn write_json_document(document: &str, output_path: Option<PathBuf>) -> Result<(), String> {
    match output_path {
        Some(output_path) => std::fs::write(&output_path, document)
            .map_err(|error| format!("failed to write {}: {error}", output_path.display())),
        None => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::new(stdout.lock());
            output
                .write_all(document.as_bytes())
                .map_err(|error| format!("failed to write output: {error}"))?;
            output
                .write_all(b"\n")
                .map_err(|error| format!("failed to write output: {error}"))?;
            output
                .flush()
                .map_err(|error| format!("failed to write output: {error}"))
        }
    }
}

fn emit_plan(
    path: PathBuf,
    parts: usize,
    strategy: &BuiltinFraming,
    options: &PlannerOptions,
    output_path: Option<PathBuf>,
) -> Result<(), String> {
    // Safety: the CLI's contract requires the input file to remain immutable while planned.
    let plan = unsafe {
        plan_file_with_framing(&path, parts, strategy, options.mode, options.window_bytes)
    }
    .map_err(|error| format!("failed to plan {}: {error}", path.display()))?;
    write_json_document(&plan.to_json(), output_path)
}

fn emit_plan_from_index(
    path: PathBuf,
    index_path: PathBuf,
    parts: usize,
    output_path: Option<PathBuf>,
) -> Result<(), String> {
    let index = RecordIndex::load(&index_path)
        .map_err(|error| format!("failed to load index {}: {error}", index_path.display()))?;
    let identity = identify_file(&path)
        .map_err(|error| format!("failed to identify {}: {error}", path.display()))?;
    if identity != index.identity {
        return Err(format!(
            "index {} does not match {} (identity mismatch); rebuild the index",
            index_path.display(),
            path.display()
        ));
    }
    let file_len = usize::try_from(identity.size)
        .map_err(|_| format!("{} is too large for this platform", path.display()))?;
    let ranges =
        plan_partition_boundaries_from_index(&index, parts, file_len).map_err(|error| {
            format!(
                "failed to derive ranges from index {}: {error}",
                index_path.display()
            )
        })?;
    let plan = plan_from_ranges(
        &path,
        &index.identity,
        index.framing.clone(),
        parts,
        IndexReference {
            stride: index.stride,
            record_count: index.record_count,
            path: Some(index_path.display().to_string()),
        },
        ranges,
    )
    .map_err(|error| {
        format!(
            "failed to build indexed plan for {}: {error}",
            path.display()
        )
    })?;
    write_json_document(&plan.to_json(), output_path)
}

fn default_index_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".mmapidx");
    PathBuf::from(name)
}

fn emit_index(
    path: PathBuf,
    strategy: &BuiltinFraming,
    every: u64,
    options: &PlannerOptions,
    output_path: Option<PathBuf>,
) -> Result<(), String> {
    // Safety: the CLI's contract requires the input file to remain immutable while indexed.
    let index = unsafe { build_record_index(&path, strategy, every, options) }
        .map_err(|error| format!("failed to index {}: {error}", path.display()))?;
    let target = output_path.unwrap_or_else(|| default_index_path(&path));
    index
        .write_json(&target)
        .map_err(|error| format!("failed to write {}: {error}", target.display()))
}

fn run_verify(manifest_path: PathBuf, file_path: PathBuf) -> Result<(), String> {
    use std::io::Read as _;

    // Bound the manifest read before parsing so the JSON DOM allocation
    // stays proportional to a fixed cap, never to attacker-sized input.
    let limit = u64::try_from(MAX_MANIFEST_BYTES)
        .map_err(|_| "manifest size limit does not fit on this platform".to_owned())?;
    let manifest_file = std::fs::File::open(&manifest_path).map_err(|error| {
        format!(
            "failed to read manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let mut text = String::new();
    manifest_file
        .take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|error| {
            format!(
                "failed to read manifest {}: {error}",
                manifest_path.display()
            )
        })?;
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "{}: invalid manifest (too_large): manifest exceeds {MAX_MANIFEST_BYTES} bytes",
            manifest_path.display()
        ));
    }
    let plan = RangePlan::from_json(&text)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;

    let live = identify_file(&file_path)
        .map_err(|error| format!("failed to identify {}: {error}", file_path.display()))?;
    if live.size != plan.source_size {
        return Err(format!(
            "stale manifest: {} records {} bytes but {} has {} bytes; re-plan the file",
            manifest_path.display(),
            plan.source_size,
            file_path.display(),
            live.size
        ));
    }
    match (&plan.identity.sample_fingerprint, &live.sample_fingerprint) {
        (Some(recorded), Some(observed)) if recorded != observed => {
            return Err(format!(
                "source content changed since {} was created ({}): fingerprint mismatch; re-plan the file",
                manifest_path.display(),
                file_path.display()
            ));
        }
        _ => {}
    }

    // Safety: the CLI's contract requires the input file to remain
    // immutable while verified. Identity is captured before and after
    // coverage and any drift discards the result; a truncation inside
    // that window can still crash via SIGBUS instead of erroring.
    let mapping = unsafe { MmapFile::open_path(&file_path) }
        .map_err(|error| format!("failed to map {}: {error}", file_path.display()))?;
    let data = unsafe { mapping.as_slice() };
    if let Err(error) = plan.verify_coverage(data) {
        return Err(format!(
            "verify failed [{}]: {}",
            error.kind.as_str(),
            error.detail
        ));
    }
    let after = identify_file(&file_path)
        .map_err(|error| format!("failed to re-identify {}: {error}", file_path.display()))?;
    if after != live {
        return Err(format!(
            "source file changed during verification ({}); result discarded",
            file_path.display()
        ));
    }
    println!(
        "verify ok: {} ranges, {} bytes, {} framing ({} v{})",
        plan.ranges.len(),
        plan.source_size,
        plan.framing.strategy_name(),
        PLAN_SCHEMA,
        PLAN_SCHEMA_VERSION
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_framing, parse_delimiter_byte, parse_delimiter_hex, parse_every, parse_framing_name,
        parse_parts, parse_prefix_bytes, parse_prefix_endian, parse_source, parse_window,
        parse_worker, FramingChoice, SourceMode, MIN_WINDOW_BYTES,
    };
    use std::ffi::OsStr;

    #[test]
    fn parts_must_be_positive() {
        assert_eq!(parse_parts(OsStr::new("1")), Ok(1));
        assert!(parse_parts(OsStr::new("0")).is_err());
        assert!(parse_parts(OsStr::new("nope")).is_err());
    }

    #[test]
    fn worker_must_be_a_non_negative_integer() {
        assert_eq!(parse_worker(OsStr::new("0")), Ok(0));
        assert_eq!(parse_worker(OsStr::new("3")), Ok(3));
        assert!(parse_worker(OsStr::new("-1")).is_err());
        assert!(parse_worker(OsStr::new("nope")).is_err());
    }

    #[test]
    fn delimiter_byte_accepts_only_decimal_u8_values() {
        assert_eq!(parse_delimiter_byte(OsStr::new("0")), Ok(0));
        assert_eq!(parse_delimiter_byte(OsStr::new("10")), Ok(10));
        assert_eq!(parse_delimiter_byte(OsStr::new("000")), Ok(0));
        assert_eq!(parse_delimiter_byte(OsStr::new("255")), Ok(255));
        assert!(parse_delimiter_byte(OsStr::new("-1")).is_err());
        assert!(parse_delimiter_byte(OsStr::new("256")).is_err());
        assert!(parse_delimiter_byte(OsStr::new("0x0a")).is_err());
        assert!(parse_delimiter_byte(OsStr::new("+10")).is_err());
        assert!(parse_delimiter_byte(OsStr::new("nope")).is_err());
    }

    #[test]
    fn delimiter_hex_accepts_even_length_hex_strings() {
        assert_eq!(parse_delimiter_hex(OsStr::new("0a")), Ok(vec![0x0A]));
        assert_eq!(
            parse_delimiter_hex(OsStr::new("0d0a")),
            Ok(vec![0x0D, 0x0A])
        );
        assert_eq!(
            parse_delimiter_hex(OsStr::new("0D0A0d0a")),
            Ok(vec![0x0D, 0x0A, 0x0D, 0x0A])
        );
        assert_eq!(parse_delimiter_hex(OsStr::new("00")), Ok(vec![0x00]));
        assert_eq!(
            parse_delimiter_hex(OsStr::new("ffFF")),
            Ok(vec![0xFF, 0xFF])
        );
    }

    #[test]
    fn delimiter_hex_rejects_invalid_forms() {
        assert!(parse_delimiter_hex(OsStr::new("")).is_err());
        assert!(parse_delimiter_hex(OsStr::new("0")).is_err());
        assert!(parse_delimiter_hex(OsStr::new("0d0")).is_err());
        assert!(parse_delimiter_hex(OsStr::new("0x0a")).is_err());
        assert!(parse_delimiter_hex(OsStr::new("zz")).is_err());
        assert!(parse_delimiter_hex(OsStr::new("0d 0a")).is_err());
        assert!(parse_delimiter_hex(OsStr::new("-1")).is_err());
    }

    #[test]
    fn source_accepts_known_backends_only() {
        assert_eq!(parse_source(OsStr::new("mmap")), Ok(SourceMode::Mmap));
        assert_eq!(
            parse_source(OsStr::new("windowed")),
            Ok(SourceMode::Windowed)
        );
        assert_eq!(parse_source(OsStr::new("pread")), Ok(SourceMode::Pread));
        assert!(parse_source(OsStr::new("Mmap")).is_err());
        assert!(parse_source(OsStr::new("file")).is_err());
        assert!(parse_source(OsStr::new("")).is_err());
    }

    #[test]
    fn window_requires_minimum_size() {
        assert_eq!(parse_window(OsStr::new("65536")), Ok(65_536));
        assert_eq!(
            parse_window(OsStr::new("67108864")),
            Ok(MIN_WINDOW_BYTES * 1024)
        );
        assert!(parse_window(OsStr::new("65535")).is_err());
        assert!(parse_window(OsStr::new("0")).is_err());
        assert!(parse_window(OsStr::new("-1")).is_err());
        assert!(parse_window(OsStr::new("64MiB")).is_err());
        assert!(parse_window(OsStr::new("nope")).is_err());
    }

    #[test]
    fn framing_name_accepts_known_modes_only() {
        assert_eq!(
            parse_framing_name(OsStr::new("delimiter")),
            Ok("delimiter".to_owned())
        );
        assert_eq!(
            parse_framing_name(OsStr::new("fixed")),
            Ok("fixed".to_owned())
        );
        assert_eq!(
            parse_framing_name(OsStr::new("length-prefixed")),
            Ok("length-prefixed".to_owned())
        );
        assert!(parse_framing_name(OsStr::new("Fixed")).is_err());
        assert!(parse_framing_name(OsStr::new("length_prefixed")).is_err());
    }

    #[test]
    fn framing_defaults_to_newline_delimiter() {
        let choice = build_framing(None, None, None, None, None, false).unwrap();
        assert_eq!(choice, FramingChoice::Delimiter(vec![0x0A]));
    }

    #[test]
    fn framing_requires_matching_options() {
        assert!(build_framing(Some("fixed"), None, None, None, None, false).is_err());
        assert!(build_framing(Some("fixed"), None, Some(16), None, None, false).is_ok());
        assert!(
            build_framing(Some("fixed"), Some(vec![0x0A]), Some(16), None, None, false).is_err()
        );
        assert!(build_framing(Some("delimiter"), None, Some(16), None, None, false).is_err());
        assert!(build_framing(Some("length-prefixed"), None, None, None, None, false).is_err());
        assert!(build_framing(Some("length-prefixed"), None, None, Some(4), None, false).is_ok());
        assert!(
            build_framing(Some("length-prefixed"), None, Some(4), Some(4), None, false).is_err()
        );
        assert!(build_framing(Some("delimiter"), None, None, Some(1), None, false).is_err());
    }

    #[test]
    fn framing_length_prefixed_defaults_to_little_endian_payload_lengths() {
        let choice =
            build_framing(Some("length-prefixed"), None, None, Some(2), None, true).unwrap();
        assert_eq!(
            choice,
            FramingChoice::LengthPrefixed {
                prefix_bytes: 2,
                little_endian: true,
                length_includes_prefix: true,
            }
        );
        let choice = build_framing(
            Some("length-prefixed"),
            None,
            None,
            Some(8),
            Some(false),
            false,
        )
        .unwrap();
        assert_eq!(
            choice,
            FramingChoice::LengthPrefixed {
                prefix_bytes: 8,
                little_endian: false,
                length_includes_prefix: false,
            }
        );
    }

    #[test]
    fn prefix_bytes_is_bounded_to_u64_width() {
        assert_eq!(parse_prefix_bytes(OsStr::new("1")), Ok(1));
        assert_eq!(parse_prefix_bytes(OsStr::new("8")), Ok(8));
        assert!(parse_prefix_bytes(OsStr::new("0")).is_err());
        assert!(parse_prefix_bytes(OsStr::new("9")).is_err());
        assert!(parse_prefix_bytes(OsStr::new("nope")).is_err());
    }

    #[test]
    fn prefix_endian_accepts_le_and_be() {
        assert_eq!(parse_prefix_endian(OsStr::new("le")), Ok(true));
        assert_eq!(parse_prefix_endian(OsStr::new("be")), Ok(false));
        assert!(parse_prefix_endian(OsStr::new("LE")).is_err());
        assert!(parse_prefix_endian(OsStr::new("little")).is_err());
    }

    #[test]
    fn every_must_be_positive() {
        assert_eq!(parse_every(OsStr::new("1")), Ok(1));
        assert_eq!(parse_every(OsStr::new("10000")), Ok(10_000));
        assert!(parse_every(OsStr::new("0")).is_err());
        assert!(parse_every(OsStr::new("nope")).is_err());
    }
}
