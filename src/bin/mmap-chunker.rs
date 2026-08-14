use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use mmap_chunker_core::MmapChunker;

const HELP: &str = "\
mmap-chunker - record-aligned byte-range planning for immutable local files

Usage:
  mmap-chunker partition FILE --parts N
  mmap-chunker --help
  mmap-chunker --version

Commands:
  partition    Emit newline-record-aligned byte ranges for FILE.

Output:
  index<TAB>start<TAB>end_exclusive<TAB>length

The default and only delimiter is newline (byte 0x0A). Offsets are bytes; starts
are inclusive and ends are exclusive. The input file must remain immutable while
it is mapped. The actual number of ranges can be lower than N when records span
multiple ideal partition positions.\n";

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
    emit_partitions(file, parts)
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

fn emit_partitions(path: PathBuf, parts: usize) -> Result<(), String> {
    // Safety: the CLI's contract requires the input file to remain immutable while mapped.
    let mut chunker = unsafe { MmapChunker::open(&path) }
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let count = chunker.partition_records(parts, b'\n');
    let source = chunker.as_bytes();
    let base = source.as_ptr();
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());

    for index in 0..count {
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
    use super::parse_parts;
    use std::ffi::OsStr;

    #[test]
    fn parts_must_be_positive() {
        assert_eq!(parse_parts(OsStr::new("1")), Ok(1));
        assert!(parse_parts(OsStr::new("0")).is_err());
        assert!(parse_parts(OsStr::new("nope")).is_err());
    }
}
