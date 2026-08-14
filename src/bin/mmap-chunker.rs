use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use mmap_chunker_core::MmapChunker;

const HELP: &str = "\
mmap-chunker - record-aligned byte-range planning for immutable local files

Usage:
  mmap-chunker partition FILE --parts N [--delimiter-byte B] [--worker K]
  mmap-chunker partition-files --parts N [--delimiter-byte B] FILE...
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
  --worker K    Emit only zero-based worker K's actual partition. K must be
                less than --parts. If record-aligned boundaries collapse and
                no actual partition K exists, the command succeeds silently.

Output:
  partition:       index<TAB>start<TAB>end_exclusive<TAB>length
  partition-files: worker<TAB>source<TAB>start<TAB>end_exclusive<TAB>length

The delimiter is one raw byte; multi-byte partition delimiters are not supported.
Offsets are bytes; starts are inclusive and ends are exclusive. The input file
must remain immutable while it is mapped. The actual number of ranges can be
lower than N when records span multiple ideal partition positions.
partition-files treats each FILE as an independent source in argument order;
it never concatenates or remaps files. Its worker rows are ordered by worker,
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
    let mut delimiter = 0x0A;
    let mut delimiter_seen = false;
    let mut worker = None;
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
                return Err("duplicate option `--delimiter-byte`".to_owned());
            }
            delimiter_seen = true;
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--delimiter-byte`".to_owned())?;
            delimiter = parse_delimiter_byte(value)?;
        } else if argument == "--worker" {
            if worker.is_some() {
                return Err("duplicate option `--worker`".to_owned());
            }
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--worker`".to_owned())?;
            worker = Some(parse_worker(value)?);
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
        emit_partitions(file, parts, delimiter, Some(worker))
    } else {
        emit_partitions(file, parts, delimiter, None)
    }
}

fn run_partition_files(arguments: &[OsString]) -> Result<(), String> {
    if arguments.len() == 1 && (arguments[0] == "--help" || arguments[0] == "-h") {
        print!("{HELP}");
        return Ok(());
    }

    let mut files = Vec::new();
    let mut parts = None;
    let mut delimiter = 0x0A;
    let mut delimiter_seen = false;
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
                return Err("duplicate option `--delimiter-byte`".to_owned());
            }
            delimiter_seen = true;
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| "missing value for `--delimiter-byte`".to_owned())?;
            delimiter = parse_delimiter_byte(value)?;
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
    emit_file_partitions(files, parts, delimiter)
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

fn emit_file_partitions(paths: Vec<PathBuf>, parts: usize, delimiter: u8) -> Result<(), String> {
    let mut sources = Vec::with_capacity(paths.len());
    for path in paths {
        // Safety: the CLI contract requires every input file to remain
        // immutable while its independent read-only mapping is live.
        let source = unsafe { MmapChunker::open(&path) }
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        sources.push(source);
    }

    let assignments = plan_logical_partitions(&sources, parts, delimiter)?;
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    for assignment in assignments {
        for range in assignment.ranges {
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

fn plan_logical_partitions(
    sources: &[MmapChunker],
    requested_parts: usize,
    delimiter: u8,
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
    let mut cut_points = Vec::with_capacity(effective_parts.saturating_sub(1));
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
        let ranges = ranges_for_interval(start, end, &prefixes, sources)?;
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
    delimiter: u8,
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
    let local_boundary = next_record_boundary(data, local_target, delimiter, state);
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

fn ranges_for_interval(
    start: u128,
    end: u128,
    prefixes: &[u128],
    sources: &[MmapChunker],
) -> Result<Vec<SourceRange>, String> {
    if start >= end {
        return Ok(Vec::new());
    }

    let mut ranges = Vec::new();
    for source_index in 0..sources.len() {
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
    use super::{parse_delimiter_byte, parse_parts, parse_worker};
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
        let assignments = super::plan_logical_partitions(&multi_sources, PARTS, b'\n').unwrap();
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
