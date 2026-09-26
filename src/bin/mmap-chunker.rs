use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mmap_chunker_core::source::{
    plan_partition_ranges, ByteSource, PlannerOptions, PreadSource, SourceMode, WindowedMmapSource,
    MIN_WINDOW_BYTES,
};
use mmap_chunker_core::MmapChunker;

const HELP: &str = "\
mmap-chunker - record-aligned byte-range planning for immutable local files

Usage:
  mmap-chunker partition FILE --parts N [--delimiter-byte B | --delimiter-hex HEX] [--source MODE] [--window BYTES] [--worker K]
  mmap-chunker partition-files --parts N [--delimiter-byte B | --delimiter-hex HEX] [--source MODE] [--window BYTES] FILE...
  mmap-chunker --help
  mmap-chunker --version

Commands:
  partition    Emit record-aligned byte ranges for FILE using one raw delimiter byte.
  partition-files
                Emit record-aligned worker/source ranges for an ordered logical dataset.

Options:
  --parts N     Request N record-aligned partitions.
  --delimiter-byte B
                Record delimiter byte in decimal (0..255). Defaults to 10
                (LF/newline). Raw byte framing only; no CSV/JSON quoting semantics.
                Mutually exclusive with --delimiter-hex. Only valid for `partition`
                and `partition-files`.
  --delimiter-hex HEX
                Record delimiter bytes as an even-length hex string, e.g.
                0d0a for CRLF. Mutually exclusive with --delimiter-byte.
                Valid for `partition` and `partition-files`.
  --source MODE Byte source backend for `partition` and `partition-files`:
                mmap (default, full-file mapping), windowed (bounded
                moving-window mapping), or pread (positional reads, no
                mapping). All backends emit identical ranges; the mode only
                changes how bytes are accessed. For `partition-files` the
                mode applies to every source; non-mmap modes open each
                source on demand and never hold more than one source open.
  --window BYTES
                Window size in bytes for --source windowed (default 67108864,
                minimum 65536). Requires --source windowed.
  --worker K    Emit only zero-based worker K's actual partition. K must be
                less than --parts. If record-aligned boundaries collapse and
                no actual partition K exists, the command succeeds silently.

Output:
  partition:       index<TAB>start<TAB>end_exclusive<TAB>length
  partition-files: worker<TAB>source<TAB>start<TAB>end_exclusive<TAB>length

The delimiter is raw byte framing only; no CSV/JSON quoting semantics.
`partition` and `partition-files` accept either `--delimiter-byte` for one
byte or `--delimiter-hex` for a multi-byte pattern (e.g. 0d0a for CRLF).
Offsets are bytes; starts are inclusive and ends are exclusive. The input file
must remain immutable while it is mapped. The actual number of ranges can be
lower than N when records span multiple ideal partition positions.
partition-files treats each FILE as an independent source in argument order;
it never concatenates or remaps files. With `--source windowed` or
`--source pread` each source is opened only while its own boundaries are
projected, so at most one source is ever held open; lengths are read up
front and rechecked on open, and a source whose length changed mid-planning
aborts the plan. Its worker rows are ordered by worker,
then source, and a worker may contain multiple source ranges. An empty FILE
contributes no rows; an all-empty dataset succeeds with empty stdout.\n";

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
        [command, rest @ ..] if command == "partition" => run_partition(rest),
        [command, rest @ ..] if command == "partition-files" => run_partition_files(rest),
        [] => Err("missing command".to_owned()),
        [command, ..] => Err(format!("unknown command `{}`", command.to_string_lossy())),
    }
}

fn run_partition(arguments: &[OsString]) -> Result<(), String> {
    if arguments.len() == 1 && (arguments[0] == "--help" || arguments[0] == "-h") {
        print!("{HELP}");
        return Ok(());
    }

    let mut file = None;
    let mut parts = None;
    let mut delimiter = vec![0x0A];
    let mut delimiter_seen = false;
    let mut worker = None;
    let mut source = SourceMode::Mmap;
    let mut source_seen = false;
    let mut window = None;
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
            delimiter = vec![parse_delimiter_byte(value)?];
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
            delimiter = parse_delimiter_hex(value)?;
        } else if argument == "--worker" {
            if worker.is_some() {
                return Err("duplicate option `--worker`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--worker`".to_owned())?;
            worker = Some(parse_worker(value)?);
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

    let file = file.ok_or_else(|| "missing FILE".to_owned())?;
    let parts = parts.ok_or_else(|| "missing required option `--parts`".to_owned())?;
    if let Some(worker) = worker {
        if worker >= parts {
            return Err("`--worker` must be less than `--parts`".to_owned());
        }
    }
    if let Some(window_bytes) = window {
        if source != SourceMode::Windowed {
            return Err("`--window` requires `--source windowed`".to_owned());
        }
        let options = PlannerOptions::new(SourceMode::Windowed).with_window_bytes(window_bytes);
        return emit_partitions_sourced(file, parts, &delimiter, worker, &options);
    }
    match source {
        SourceMode::Mmap => emit_partitions_for_delimiter(file, parts, &delimiter, worker),
        SourceMode::Windowed => {
            let options = PlannerOptions::new(SourceMode::Windowed);
            emit_partitions_sourced(file, parts, &delimiter, worker, &options)
        }
        SourceMode::Pread => {
            let options = PlannerOptions::new(SourceMode::Pread);
            emit_partitions_sourced(file, parts, &delimiter, worker, &options)
        }
    }
}

fn run_partition_files(arguments: &[OsString]) -> Result<(), String> {
    if arguments.len() == 1 && (arguments[0] == "--help" || arguments[0] == "-h") {
        print!("{HELP}");
        return Ok(());
    }

    let mut files = Vec::new();
    let mut parts = None;
    let mut delimiter = vec![0x0A];
    let mut delimiter_seen = false;
    let mut source = SourceMode::Mmap;
    let mut source_seen = false;
    let mut window = None;
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
            delimiter = vec![parse_delimiter_byte(value)?];
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
            delimiter = parse_delimiter_hex(value)?;
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
        } else if argument.as_os_str().to_string_lossy().starts_with('-') {
            return Err(format!(
                "unexpected option `{}`",
                argument.to_string_lossy()
            ));
        } else {
            files.push(PathBuf::from(argument));
        }
        index += 1;
    }

    let parts = parts.ok_or_else(|| "missing required option `--parts`".to_owned())?;
    if files.is_empty() {
        return Err("missing FILE (provide one or more ordered source paths)".to_owned());
    }
    if let Some(window_bytes) = window {
        if source != SourceMode::Windowed {
            return Err("`--window` requires `--source windowed`".to_owned());
        }
        let options = PlannerOptions::new(SourceMode::Windowed).with_window_bytes(window_bytes);
        return emit_file_partitions_sourced(files, parts, &delimiter, &options);
    }
    match source {
        // The default mmap engine path is unchanged: identical behavior by
        // construction, including for an explicit `--source mmap`.
        SourceMode::Mmap => emit_file_partitions(files, parts, &delimiter),
        SourceMode::Windowed => {
            let options = PlannerOptions::new(SourceMode::Windowed);
            emit_file_partitions_sourced(files, parts, &delimiter, &options)
        }
        SourceMode::Pread => {
            let options = PlannerOptions::new(SourceMode::Pread);
            emit_file_partitions_sourced(files, parts, &delimiter, &options)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SourceRange {
    source_index: usize,
    start: usize,
    end_exclusive: usize,
    length: usize,
}

#[derive(Debug)]
struct WorkerAssignment {
    worker_index: usize,
    ranges: Vec<SourceRange>,
}

#[derive(Clone, Copy, Debug, Default)]
struct BoundarySearchState {
    scan_from: usize,
    cached_boundary: Option<usize>,
}

fn emit_file_partitions(paths: Vec<PathBuf>, parts: usize, delimiter: &[u8]) -> Result<(), String> {
    let mut sources = Vec::with_capacity(paths.len());
    for path in paths {
        // Safety: the CLI contract requires every input file to remain
        // immutable while its independent read-only mapping is live.
        let source = unsafe { MmapChunker::open(&path) }
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        sources.push(source);
    }

    let assignments = plan_logical_partitions(&sources, parts, delimiter)?;
    write_assignments(&assignments)
}

fn write_assignments(assignments: &[WorkerAssignment]) -> Result<(), String> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    for assignment in assignments {
        for range in &assignment.ranges {
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}",
                assignment.worker_index,
                range.source_index,
                range.start,
                range.end_exclusive,
                range.length
            )
            .map_err(|error| format!("failed to write output: {error}"))?;
        }
    }
    output
        .flush()
        .map_err(|error| format!("failed to write output: {error}"))
}

/// One on-demand source held open while its own targets are projected.
///
/// Planning keeps at most one of these alive at a time, so the peak held
/// resources are one open source plus one scan buffer (and at most one
/// live window for `SourceMode::Windowed`; nothing mapped for
/// `SourceMode::Pread`).
struct OpenSourced {
    index: usize,
    source: Box<dyn ByteSource>,
    state: BoundarySearchState,
    buffer: Vec<u8>,
}

fn open_sourced(path: &Path, options: &PlannerOptions) -> Result<Box<dyn ByteSource>, String> {
    // Safety: the CLI contract requires every input file to remain
    // immutable while it is planned, matching single-file sourced planning.
    // `SourceMode::Mmap` never reaches this path; it uses the legacy
    // full-mapping engine so default behavior is identical by construction.
    match options.mode {
        SourceMode::Mmap => Err("internal error: mmap uses the legacy engine path".to_owned()),
        SourceMode::Windowed => {
            unsafe { WindowedMmapSource::open_path(path, options.window_bytes) }
                .map(|source| Box::new(source) as Box<dyn ByteSource>)
                .map_err(|error| format!("failed to open {}: {error}", path.display()))
        }
        SourceMode::Pread => PreadSource::open_path(path)
            .map(|source| Box::new(source) as Box<dyn ByteSource>)
            .map_err(|error| format!("failed to open {}: {error}", path.display())),
    }
}

fn emit_file_partitions_sourced(
    paths: Vec<PathBuf>,
    parts: usize,
    delimiter: &[u8],
    options: &PlannerOptions,
) -> Result<(), String> {
    let assignments = plan_logical_partitions_sourced(&paths, parts, delimiter, options)?;
    write_assignments(&assignments)
}

fn plan_logical_partitions_sourced(
    paths: &[PathBuf],
    requested_parts: usize,
    delimiter: &[u8],
    options: &PlannerOptions,
) -> Result<Vec<WorkerAssignment>, String> {
    if requested_parts == 0 {
        return Ok(Vec::new());
    }

    // Phase 1: lengths only — no mappings, no held handles. Every path is
    // statted up front so a missing file fails before any output, matching
    // the legacy open-everything-first atomicity.
    let mut prefixes = Vec::with_capacity(paths.len() + 1);
    prefixes.push(0u128);
    for path in paths {
        let len = std::fs::metadata(path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?
            .len();
        let previous = *prefixes
            .last()
            .ok_or_else(|| "internal error: missing logical prefix".to_owned())?;
        let next = previous
            .checked_add(u128::from(len))
            .ok_or_else(|| "logical dataset is too large".to_owned())?;
        prefixes.push(next);
    }
    let total = *prefixes
        .last()
        .ok_or_else(|| "internal error: missing logical length".to_owned())?;
    if total == 0 {
        return Ok(Vec::new());
    }

    // Match the legacy planner's bounded request behavior whenever the
    // logical byte count fits usize. All arithmetic for target positions is
    // still performed in u128 so many independent files compose safely.
    let effective_parts = requested_parts.min(usize::try_from(total).unwrap_or(usize::MAX));
    // Every search window holds at least one complete pattern.
    let buffer_len = options.scan_buffer_bytes.max(delimiter.len()).max(1);
    // Do not reserve one entry per requested partition up front: a giant
    // record or a sparse delimiter layout may collapse almost every target.
    let mut cut_points = Vec::new();
    let mut last_cut = 0u128;
    // Logical targets increase monotonically, so per-source targets do too;
    // once a source is closed it is never reopened.
    let mut current: Option<OpenSourced> = None;

    for partition in 1..effective_parts {
        let target = total
            .checked_mul(usize_to_u128(partition)?)
            .ok_or_else(|| "logical target arithmetic overflow".to_owned())?
            / usize_to_u128(effective_parts)?;
        if target <= last_cut {
            continue;
        }

        let projected = project_sourced_target(
            target,
            &prefixes,
            paths,
            delimiter,
            options,
            &mut current,
            buffer_len,
        )?;
        if projected <= last_cut {
            continue;
        }
        if projected == total {
            break;
        }
        cut_points.push(projected);
        last_cut = projected;
    }

    let mut assignments = Vec::with_capacity(cut_points.len() + 1);
    let mut start = 0u128;
    for end in cut_points.into_iter().chain(std::iter::once(total)) {
        let ranges = ranges_for_interval(start, end, &prefixes, paths.len())?;
        if !ranges.is_empty() {
            assignments.push(WorkerAssignment {
                worker_index: assignments.len(),
                ranges,
            });
        }
        start = end;
    }
    Ok(assignments)
}

fn project_sourced_target(
    target: u128,
    prefixes: &[u128],
    paths: &[PathBuf],
    delimiter: &[u8],
    options: &PlannerOptions,
    current: &mut Option<OpenSourced>,
    buffer_len: usize,
) -> Result<u128, String> {
    // A source boundary is always a valid logical boundary, including the
    // boundaries around empty sources.
    if prefixes.binary_search(&target).is_ok() {
        return Ok(target);
    }

    let upper = prefixes.partition_point(|prefix| *prefix <= target);
    let source_index = upper
        .checked_sub(1)
        .ok_or_else(|| "internal error: target before first source".to_owned())?;
    let source_start = prefixes[source_index];
    let expected_len = prefixes[source_index + 1] - source_start;
    let local_target = usize::try_from(target - source_start)
        .map_err(|_| "logical target does not fit source offset".to_owned())?;
    let path = paths
        .get(source_index)
        .ok_or_else(|| "internal error: target source out of bounds".to_owned())?;

    if current.as_ref().map(|open| open.index) != Some(source_index) {
        // Drop the previous source *before* opening the next one so at most
        // one source is ever held open.
        *current = None;
        let source = open_sourced(path, options)?;
        // The immutable-input contract forbids length changes between the
        // phase-1 stat and this open; fail closed instead of planning
        // against stale prefixes.
        if usize_to_u128(source.len())? != expected_len {
            return Err(format!(
                "source length changed during planning: {}",
                path.display()
            ));
        }
        *current = Some(OpenSourced {
            index: source_index,
            source,
            state: BoundarySearchState::default(),
            buffer: vec![0u8; buffer_len],
        });
    }
    let open = current
        .as_mut()
        .ok_or_else(|| "internal error: source not open".to_owned())?;
    let local_boundary = next_record_boundary_sourced(
        open.source.as_ref(),
        local_target,
        delimiter,
        &mut open.state,
        &mut open.buffer,
        path,
    )?;
    Ok(source_start + usize_to_u128(local_boundary)?)
}

/// Fill `buf` from `offset`, tolerating short reads (`read_at` may return
/// fewer bytes than requested). Returns the bytes accumulated; `0` means
/// the source is exhausted at `offset`.
fn fill_from(source: &dyn ByteSource, offset: usize, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let read = source.read_at(offset + filled, &mut buf[filled..])?;
        if read == 0 {
            break;
        }
        filled = filled.saturating_add(read);
    }
    Ok(filled)
}

fn next_record_boundary_sourced(
    source: &dyn ByteSource,
    target: usize,
    delimiter: &[u8],
    state: &mut BoundarySearchState,
    buffer: &mut [u8],
    path: &Path,
) -> Result<usize, String> {
    if let Some(cached_boundary) = state.cached_boundary {
        if target < cached_boundary {
            return Ok(cached_boundary);
        }
    }

    let file_len = source.len();
    let read_error = |error: std::io::Error| format!("failed to read {}: {error}", path.display());
    if delimiter.len() == 1 {
        // No exact-target acceptance for single-byte: matches the legacy
        // path, which the differential matrix proves byte-identical.
        let byte = delimiter[0];
        let scan_from = target.max(state.scan_from);
        let mut offset = scan_from;
        let boundary = loop {
            if offset >= file_len {
                break file_len;
            }
            let want = buffer.len().min(file_len - offset);
            let filled = fill_from(source, offset, &mut buffer[..want]).map_err(&read_error)?;
            if filled == 0 {
                break file_len;
            }
            if let Some(relative) = buffer[..filled].iter().position(|b| *b == byte) {
                break offset
                    .saturating_add(relative)
                    .saturating_add(1)
                    .min(file_len);
            }
            if offset.saturating_add(filled) >= file_len {
                break file_len;
            }
            offset = offset.saturating_add(filled);
        };
        state.scan_from = boundary;
        state.cached_boundary = Some(boundary);
        return Ok(boundary);
    }

    // Mirror the multi-byte exact-target semantics: a target that already
    // follows a complete delimiter pattern is accepted exactly.
    let dlen = delimiter.len();
    if target >= dlen && target <= file_len {
        let filled = fill_from(source, target - dlen, &mut buffer[..dlen]).map_err(&read_error)?;
        if filled == dlen && buffer[..dlen] == delimiter[..] {
            state.scan_from = target.max(state.scan_from);
            state.cached_boundary = Some(target);
            return Ok(target);
        }
    }

    let scan_from = target.max(state.scan_from);
    let mut offset = scan_from;
    let boundary = loop {
        if offset >= file_len {
            break file_len;
        }
        let want = buffer.len().min(file_len - offset);
        let filled = fill_from(source, offset, &mut buffer[..want]).map_err(&read_error)?;
        if filled == 0 {
            break file_len;
        }
        // The advance below re-reads the last `dlen - 1` bytes, so a
        // pattern spanning two reads still appears whole in some buffer;
        // matching inside filled bytes (overlap-safe, one-byte steps)
        // therefore has the same observable contract as the slice search.
        if filled >= dlen {
            if let Some(relative) = find_first_pattern_from(&buffer[..filled], delimiter) {
                break offset
                    .saturating_add(relative)
                    .saturating_add(dlen)
                    .min(file_len);
            }
        }
        if offset.saturating_add(filled) >= file_len {
            break file_len;
        }
        offset = offset.saturating_add(filled.saturating_sub(dlen.saturating_sub(1)).max(1));
    };
    state.scan_from = boundary;
    state.cached_boundary = Some(boundary);
    Ok(boundary)
}

fn plan_logical_partitions(
    sources: &[MmapChunker],
    requested_parts: usize,
    delimiter: &[u8],
) -> Result<Vec<WorkerAssignment>, String> {
    if requested_parts == 0 {
        return Ok(Vec::new());
    }

    let mut prefixes = Vec::with_capacity(sources.len() + 1);
    prefixes.push(0u128);
    for source in sources {
        let previous = *prefixes
            .last()
            .ok_or_else(|| "internal error: missing logical prefix".to_owned())?;
        let next = previous
            .checked_add(usize_to_u128(source.len())?)
            .ok_or_else(|| "logical dataset is too large".to_owned())?;
        prefixes.push(next);
    }
    let total = *prefixes
        .last()
        .ok_or_else(|| "internal error: missing logical length".to_owned())?;
    if total == 0 {
        return Ok(Vec::new());
    }

    // Match the single-file planner's bounded request behavior whenever the
    // logical byte count fits usize. All arithmetic for target positions is
    // still performed in u128 so many independent files compose safely.
    let effective_parts = requested_parts.min(usize::try_from(total).unwrap_or(usize::MAX));
    let mut states = vec![BoundarySearchState::default(); sources.len()];
    // Do not reserve one entry per requested partition up front: a giant
    // record or a sparse delimiter layout may collapse almost every target.
    let mut cut_points = Vec::new();
    let mut last_cut = 0u128;

    for partition in 1..effective_parts {
        let target = total
            .checked_mul(usize_to_u128(partition)?)
            .ok_or_else(|| "logical target arithmetic overflow".to_owned())?
            / usize_to_u128(effective_parts)?;
        if target <= last_cut {
            continue;
        }

        let projected = project_logical_target(target, &prefixes, sources, delimiter, &mut states)?;
        if projected <= last_cut {
            continue;
        }
        if projected == total {
            break;
        }
        cut_points.push(projected);
        last_cut = projected;
    }

    let mut assignments = Vec::with_capacity(cut_points.len() + 1);
    let mut start = 0u128;
    for end in cut_points.into_iter().chain(std::iter::once(total)) {
        let ranges = ranges_for_interval(start, end, &prefixes, sources.len())?;
        if !ranges.is_empty() {
            assignments.push(WorkerAssignment {
                worker_index: assignments.len(),
                ranges,
            });
        }
        start = end;
    }
    Ok(assignments)
}

fn project_logical_target(
    target: u128,
    prefixes: &[u128],
    sources: &[MmapChunker],
    delimiter: &[u8],
    states: &mut [BoundarySearchState],
) -> Result<u128, String> {
    // A source boundary is always a valid logical boundary, including the
    // boundaries around empty sources.
    if prefixes.binary_search(&target).is_ok() {
        return Ok(target);
    }

    let upper = prefixes.partition_point(|prefix| *prefix <= target);
    let source_index = upper
        .checked_sub(1)
        .ok_or_else(|| "internal error: target before first source".to_owned())?;
    let source_start = prefixes[source_index];
    let local_target = usize::try_from(target - source_start)
        .map_err(|_| "logical target does not fit source offset".to_owned())?;
    let data = sources
        .get(source_index)
        .ok_or_else(|| "internal error: target source out of bounds".to_owned())?
        .as_bytes();
    let state = states
        .get_mut(source_index)
        .ok_or_else(|| "internal error: boundary state out of bounds".to_owned())?;
    let local_boundary = if delimiter.len() == 1 {
        // Byte-identical to the pre-existing single-byte path.
        next_record_boundary(data, local_target, delimiter[0], state)
    } else {
        next_record_boundary_pattern(data, local_target, delimiter, state)
    };
    Ok(source_start + usize_to_u128(local_boundary)?)
}

fn usize_to_u128(value: usize) -> Result<u128, String> {
    u64::try_from(value)
        .map(u128::from)
        .map_err(|_| "usize value does not fit in the logical offset type".to_owned())
}

fn next_record_boundary(
    data: &[u8],
    target: usize,
    delimiter: u8,
    state: &mut BoundarySearchState,
) -> usize {
    if let Some(cached_boundary) = state.cached_boundary {
        if target < cached_boundary {
            return cached_boundary;
        }
    }

    let scan_from = target.max(state.scan_from);
    let boundary = data[scan_from..]
        .iter()
        .position(|byte| *byte == delimiter)
        .map(|relative| {
            scan_from
                .saturating_add(relative)
                .saturating_add(1)
                .min(data.len())
        })
        .unwrap_or(data.len());
    state.scan_from = boundary;
    state.cached_boundary = Some(boundary);
    boundary
}

fn next_record_boundary_pattern(
    data: &[u8],
    target: usize,
    delimiter: &[u8],
    state: &mut BoundarySearchState,
) -> usize {
    debug_assert!(delimiter.len() > 1);
    if let Some(cached_boundary) = state.cached_boundary {
        if target < cached_boundary {
            return cached_boundary;
        }
    }

    // Mirror `scanner::find_partition_boundaries_pattern` exact-target
    // semantics: a target that already follows a complete delimiter pattern
    // is a valid record boundary and is accepted exactly.
    if target >= delimiter.len()
        && target <= data.len()
        && data[target - delimiter.len()..target] == delimiter[..]
    {
        state.scan_from = target.max(state.scan_from);
        state.cached_boundary = Some(target);
        return target;
    }

    let scan_from = target.max(state.scan_from);
    let boundary = if scan_from >= data.len() {
        data.len()
    } else {
        // First occurrence of the complete pattern at or after `scan_from`
        // (overlap-safe: candidates advance by one byte). This is the same
        // observable contract as the library's SWAR-accelerated
        // `find_pattern_in_slice`; the CLI keeps a dependency-free local
        // loop because the binary crate cannot see the library's
        // `pub(crate)` primitive and the pattern lengths here are tiny.
        find_first_pattern_from(&data[scan_from..], delimiter)
            .map(|relative| {
                scan_from
                    .saturating_add(relative)
                    .saturating_add(delimiter.len())
                    .min(data.len())
            })
            .unwrap_or(data.len())
    };
    state.scan_from = boundary;
    state.cached_boundary = Some(boundary);
    boundary
}

/// First offset of `pattern` in `haystack`, or `None`.
///
/// Overlap-safe linear scan with the same observable contract as
/// `scanner::find_pattern_in_slice` (first occurrence; empty pattern and
/// short haystack yield `None`). Callers guarantee a non-empty pattern.
fn find_first_pattern_from(haystack: &[u8], pattern: &[u8]) -> Option<usize> {
    if pattern.is_empty() || haystack.len() < pattern.len() {
        return None;
    }
    (0..=haystack.len() - pattern.len()).find(|offset| haystack[*offset..].starts_with(pattern))
}

fn ranges_for_interval(
    start: u128,
    end: u128,
    prefixes: &[u128],
    source_count: usize,
) -> Result<Vec<SourceRange>, String> {
    if start >= end {
        return Ok(Vec::new());
    }

    let mut ranges = Vec::new();
    for source_index in 0..source_count {
        let source_start = prefixes[source_index];
        let source_end = prefixes[source_index + 1];
        let range_start = start.max(source_start);
        let range_end = end.min(source_end);
        if range_start >= range_end {
            continue;
        }

        let local_start = usize::try_from(range_start - source_start)
            .map_err(|_| "source range start does not fit usize".to_owned())?;
        let local_end = usize::try_from(range_end - source_start)
            .map_err(|_| "source range end does not fit usize".to_owned())?;
        let length = local_end
            .checked_sub(local_start)
            .ok_or_else(|| "source range length underflow".to_owned())?;
        ranges.push(SourceRange {
            source_index,
            start: local_start,
            end_exclusive: local_end,
            length,
        });
    }
    Ok(ranges)
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
    let parsed = value
        .to_str()
        .ok_or_else(|| "`--window` must be a positive integer".to_owned())?
        .parse::<usize>()
        .map_err(|_| {
            format!(
                "invalid value for `--window`: `{}`",
                value.to_string_lossy()
            )
        })?;
    if parsed < MIN_WINDOW_BYTES {
        return Err(format!(
            "`--window` must be at least {MIN_WINDOW_BYTES} bytes"
        ));
    }
    Ok(parsed)
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

fn emit_partitions_for_delimiter(
    path: PathBuf,
    parts: usize,
    delimiter: &[u8],
    worker: Option<usize>,
) -> Result<(), String> {
    // A single-byte delimiter takes the exact pre-v1.4 single-byte path;
    // longer patterns use record-aligned multi-byte partitioning.
    if delimiter.len() == 1 {
        emit_partitions(path, parts, delimiter[0], worker)
    } else {
        emit_partitions_pattern(path, parts, delimiter, worker)
    }
}

fn emit_partitions(
    path: PathBuf,
    parts: usize,
    delimiter: u8,
    worker: Option<usize>,
) -> Result<(), String> {
    // Safety: the CLI's contract requires the input file to remain immutable while mapped.
    let mut chunker = unsafe { MmapChunker::open(&path) }
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let count = chunker.partition_records(parts, delimiter);
    emit_indexed_chunks(&chunker, count, worker)
}

fn emit_partitions_pattern(
    path: PathBuf,
    parts: usize,
    delimiter: &[u8],
    worker: Option<usize>,
) -> Result<(), String> {
    // Safety: the CLI's contract requires the input file to remain immutable while mapped.
    let mut chunker = unsafe { MmapChunker::open(&path) }
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let count = chunker.partition_records_pattern(parts, delimiter);
    emit_indexed_chunks(&chunker, count, worker)
}

fn emit_partitions_sourced(
    path: PathBuf,
    parts: usize,
    delimiter: &[u8],
    worker: Option<usize>,
    options: &PlannerOptions,
) -> Result<(), String> {
    // Safety: the CLI's contract requires the input file to remain immutable
    // while it is accessed (mmap-backed modes map it read-only).
    let ranges = unsafe { plan_partition_ranges(&path, parts, delimiter, options) }
        .map_err(|error| format!("failed to plan {}: {error}", path.display()))?;
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());

    let indices: Vec<usize> = match worker {
        Some(index) if index < ranges.len() => vec![index],
        Some(_) => Vec::new(),
        None => (0..ranges.len()).collect(),
    };
    for index in indices {
        let (start, end) = ranges[index];
        writeln!(output, "{index}\t{start}\t{end}\t{}", end - start)
            .map_err(|error| format!("failed to write output: {error}"))?;
    }
    output
        .flush()
        .map_err(|error| format!("failed to write output: {error}"))
}

fn emit_indexed_chunks(
    chunker: &MmapChunker,
    count: usize,
    worker: Option<usize>,
) -> Result<(), String> {
    let source = chunker.as_bytes();
    let base = source.as_ptr();
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());

    let indices = match worker {
        Some(index) if index < count => index..index + 1,
        Some(_) => 0..0,
        None => 0..count,
    };
    for index in indices {
        let chunk = chunker
            .get_chunk(index)
            .expect("partition count must match accessible chunks");
        // Both pointers are derived from the same mapped byte slice.
        let start = unsafe { chunk.as_ptr().offset_from(base) };
        let start = usize::try_from(start).expect("chunk start must not precede mapped bytes");
        let end = start
            .checked_add(chunk.len())
            .expect("chunk end must fit in usize");
        writeln!(output, "{index}\t{start}\t{end}\t{}", chunk.len())
            .map_err(|error| format!("failed to write output: {error}"))?;
    }
    output
        .flush()
        .map_err(|error| format!("failed to write output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{parse_delimiter_byte, parse_delimiter_hex, parse_parts, parse_worker};
    use mmap_chunker_core::MmapChunker;
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
    #[ignore = "bounded local multi-file planning proof"]
    fn bounded_multi_file_planning_proof() {
        use std::time::Instant;

        const FILE_COUNT: usize = 100;
        const FILE_SIZE: usize = 1024 * 1024;
        const PARTS: usize = 8;
        let directory = std::env::temp_dir().join(format!(
            "mmap_chunker_multi_file_bounded_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();

        let mut paths = Vec::with_capacity(FILE_COUNT);
        let mut fixture = vec![b'x'; FILE_SIZE];
        for offset in (4095..FILE_SIZE).step_by(4096) {
            fixture[offset] = b'\n';
        }
        for index in 0..FILE_COUNT {
            let path = directory.join(format!("source-{index}.jsonl"));
            std::fs::write(&path, &fixture).unwrap();
            paths.push(path);
        }

        let mut multi_sources = Vec::with_capacity(FILE_COUNT);
        for path in &paths {
            multi_sources.push(unsafe { MmapChunker::open(path).unwrap() });
        }
        let multi_started = Instant::now();
        let assignments = super::plan_logical_partitions(&multi_sources, PARTS, b"\n").unwrap();
        let multi_elapsed = multi_started.elapsed();
        assert!(!assignments.is_empty());

        let mut single_sources = Vec::with_capacity(FILE_COUNT);
        for path in &paths {
            single_sources.push(unsafe { MmapChunker::open(path).unwrap() });
        }
        let single_started = Instant::now();
        for source in &mut single_sources {
            assert!(source.partition_records(PARTS, b'\n') > 0);
        }
        let single_elapsed = single_started.elapsed();
        eprintln!(
            "bounded multi-file planning: files={} total_mib={} multi_ms={:.3} single_sum_ms={:.3} ratio={:.3}",
            FILE_COUNT,
            (FILE_COUNT * FILE_SIZE) / (1024 * 1024),
            multi_elapsed.as_secs_f64() * 1000.0,
            single_elapsed.as_secs_f64() * 1000.0,
            multi_elapsed.as_secs_f64() / single_elapsed.as_secs_f64().max(f64::EPSILON)
        );

        drop(single_sources);
        drop(multi_sources);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
