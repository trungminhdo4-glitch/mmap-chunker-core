//! File-identity pinning: mechanical enforcement of the immutable-input contract.
//!
//! A [`PinnedFile`] snapshots the identity of a path at open time (size,
//! modification time, plus a platform file key) using [`std::fs::metadata`]
//! and re-stats the same path before every scan/plan operation. Any visible
//! change fails closed.
//!
//! Platform keys:
//!
//! * Unix: `(dev, ino)` via [`std::os::unix::fs::MetadataExt`] (long-stable).
//! * Windows and other platforms: size + mtime only (documented fallback).
//!
//!   Windows has no usable stable file key: `volume_serial_number` /
//!   `file_index` require the unstable `windows_by_handle` feature
//!   (rust-lang issue #63010, forbidden by the MSRV 1.77 / stable-toolchain
//!   gate), and creation time is disqualified empirically — Windows file
//!   timestamps tick at timer granularity, so two files created in quick
//!   succession routinely share identical creation times, and NTFS
//!   tunneling can preserve them across replacement. Probing on the build
//!   host showed back-to-back creates with equal creation stamps and
//!   rename-over preserving them. A same-size/same-mtime swap is therefore
//!   undecidable on Windows (as on other fallback targets).
//!
//! Known limitation: a rotation that preserves size, mtime, and the platform
//! key (or where the key is unavailable) is undecidable by stat alone.
//! Detecting that case requires content hash-anchoring, which is explicitly
//! future work and is NOT implemented here. [`PinMismatchKind::Rotated`] is
//! reserved for that future detector and is never returned by
//! [`PinnedFile::revalidate`] today.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Check only the file size class.
pub const PIN_SIZE: u32 = 1 << 0;
/// Check only the platform identity class (`dev/ino` on Unix; no key on
/// Windows/other targets, where the flag is accepted but has nothing to
/// compare — see the module docs).
pub const PIN_IDENTITY: u32 = 1 << 1;
/// Check only the modification-time class.
pub const PIN_MTIME: u32 = 1 << 2;

/// Convenience mask of all known pin classes.
pub const PIN_ALL: u32 = PIN_SIZE | PIN_IDENTITY | PIN_MTIME;

/// Mask of all flag bits this version understands (forward-compat gate).
const PIN_KNOWN_MASK: u32 = PIN_ALL;

/// Validate raw pin flags, mapping `0` to "all classes".
///
/// Returns the effective mask, or an [`io::Error`] with
/// [`io::ErrorKind::InvalidInput`] when unknown bits are set (fail closed
/// for forward compatibility).
fn effective_flags(flags: u32) -> io::Result<u32> {
    if flags & !PIN_KNOWN_MASK != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown pin flag bits: {:#x}", flags & !PIN_KNOWN_MASK),
        ));
    }
    if flags == 0 {
        Ok(PIN_ALL)
    } else {
        Ok(flags)
    }
}

/// Opaque-ish snapshot of a file's identity at pin time.
#[derive(Debug, Clone)]
pub struct FileIdentity {
    size: u64,
    mtime: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileIdentity {
    /// File size in bytes at capture time.
    #[inline]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Modification time at capture time, or `None` when unavailable.
    #[inline]
    pub fn mtime(&self) -> Option<SystemTime> {
        self.mtime
    }

    /// Device id at capture time (Unix only).
    #[cfg(unix)]
    #[inline]
    pub fn dev(&self) -> u64 {
        self.dev
    }

    /// Inode number at capture time (Unix only).
    #[cfg(unix)]
    #[inline]
    pub fn ino(&self) -> u64 {
        self.ino
    }

    fn capture_from_metadata(meta: &std::fs::Metadata) -> Self {
        let size = meta.len();
        let mtime = meta.modified().ok();
        Self {
            size,
            mtime,
            #[cfg(unix)]
            dev: {
                use std::os::unix::fs::MetadataExt;
                meta.dev()
            },
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                meta.ino()
            },
        }
    }
}

/// Classification of a pin revalidation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMismatchKind {
    /// Reserved for future hash-anchored rotation detection. Never returned
    /// by [`PinnedFile::revalidate`] today (see module docs).
    Rotated,
    /// Platform file key changed (`dev/ino` on Unix). Covers atomic
    /// rename-over replacement. Windows/other targets have no stable key,
    /// so this variant is Unix-only in practice.
    Replaced,
    /// File size changed (any direction; the name reflects the dominant
    /// truncation hazard for mmap consumers).
    Truncated,
    /// Modification time changed while size and identity matched.
    MtimeJump,
    /// Path is missing, unstatable, or the stored flags are corrupt.
    /// Fail-closed bucket.
    MetadataUnavailable,
}

impl fmt::Display for PinMismatchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Rotated => "rotated",
            Self::Replaced => "replaced",
            Self::Truncated => "truncated",
            Self::MtimeJump => "mtime-jump",
            Self::MetadataUnavailable => "metadata-unavailable",
        };
        write!(f, "{name}")
    }
}

/// A failed [`PinnedFile::revalidate`] with a human-readable detail.
#[derive(Debug, Clone)]
pub struct PinMismatch {
    kind: PinMismatchKind,
    detail: String,
}

impl PinMismatch {
    /// Machine-readable classification.
    #[inline]
    pub fn kind(&self) -> PinMismatchKind {
        self.kind
    }

    /// Human-readable detail (stable across calls, no paths with NUL).
    #[inline]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Convert to an [`io::Error`] with [`io::ErrorKind::InvalidData`].
    ///
    /// `InvalidData` (not `Other`) is chosen so callers can distinguish
    /// "file changed under a live handle" from generic I/O failures without
    /// parsing strings; the `Display` text is preserved as the error
    /// message.
    pub fn to_io_error(&self) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, self.to_string())
    }
}

impl fmt::Display for PinMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "file identity changed ({}): {}", self.kind, self.detail)
    }
}

impl std::error::Error for PinMismatch {}

impl From<PinMismatch> for io::Error {
    fn from(m: PinMismatch) -> io::Error {
        m.to_io_error()
    }
}

/// A path pinned to the [`FileIdentity`] observed at capture time.
#[derive(Debug, Clone)]
pub struct PinnedFile {
    path: PathBuf,
    identity: FileIdentity,
    flags: u32,
}

impl PinnedFile {
    /// Snapshot `path` via [`std::fs::metadata`] (no raw syscalls).
    ///
    /// `flags` selects which classes [`revalidate`](Self::revalidate)
    /// checks (`PIN_SIZE` / `PIN_IDENTITY` / `PIN_MTIME`); `0` means all.
    /// Unknown flag bits fail closed with [`io::ErrorKind::InvalidInput`].
    pub fn capture(path: impl AsRef<Path>, flags: u32) -> io::Result<Self> {
        effective_flags(flags)?;
        let path_buf = path.as_ref().to_path_buf();
        let meta = std::fs::metadata(&path_buf)?;
        Ok(Self {
            path: path_buf,
            identity: FileIdentity::capture_from_metadata(&meta),
            flags,
        })
    }

    /// The pinned path (as captured).
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The snapshot taken at capture time.
    #[inline]
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }

    /// Raw flags passed to [`capture`](Self::capture) (`0` means all).
    #[inline]
    pub fn flags(&self) -> u32 {
        self.flags
    }

    /// Re-stat the pinned path and compare against the snapshot.
    ///
    /// Only classes enabled by the capture flags are checked (`0` means
    /// all). Precedence (first match wins, fail closed):
    ///
    /// 1. Path missing or unstatable -> [`PinMismatchKind::MetadataUnavailable`].
    /// 2. Platform key differs (Unix `dev/ino` only) ->
    ///    [`PinMismatchKind::Replaced`] (covers atomic rename-over).
    /// 3. Size differs -> [`PinMismatchKind::Truncated`].
    /// 4. Mtime differs (both present and unequal) -> [`PinMismatchKind::MtimeJump`].
    ///
    /// A rotation preserving size, mtime, and (on Unix) key is undecidable
    /// by stat alone and is NOT reported (hash-anchoring is future work).
    /// On Windows/other targets the identity class has no key and passes;
    /// a same-size/same-mtime swap there is likewise undecidable.
    pub fn revalidate(&self) -> Result<(), PinMismatch> {
        let effective = match effective_flags(self.flags) {
            Ok(mask) => mask,
            Err(e) => {
                return Err(PinMismatch {
                    kind: PinMismatchKind::MetadataUnavailable,
                    detail: format!("invalid pin flags: {e}"),
                });
            }
        };
        let meta = std::fs::metadata(&self.path).map_err(|e| PinMismatch {
            kind: PinMismatchKind::MetadataUnavailable,
            detail: format!("cannot stat pinned path: {e}"),
        })?;
        let current = FileIdentity::capture_from_metadata(&meta);

        if effective & PIN_IDENTITY != 0 {
            #[cfg(unix)]
            {
                if current.dev != self.identity.dev || current.ino != self.identity.ino {
                    return Err(PinMismatch {
                        kind: PinMismatchKind::Replaced,
                        detail: format!(
                            "file key changed (dev/ino {}:{} -> {}:{})",
                            self.identity.dev, self.identity.ino, current.dev, current.ino
                        ),
                    });
                }
            }
            #[cfg(not(unix))]
            {
                // No stable file key on Windows/other targets (see module
                // docs): the identity class passes; size+mtime still apply.
            }
        }

        if effective & PIN_SIZE != 0 && current.size != self.identity.size {
            return Err(PinMismatch {
                kind: PinMismatchKind::Truncated,
                detail: format!(
                    "file size changed ({} -> {})",
                    self.identity.size, current.size
                ),
            });
        }

        if effective & PIN_MTIME != 0 {
            match (self.identity.mtime, current.mtime) {
                (Some(old), Some(new)) if old != new => {
                    return Err(PinMismatch {
                        kind: PinMismatchKind::MtimeJump,
                        detail: "modification time changed".to_string(),
                    });
                }
                _ => {}
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::{Duration, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mmap_chunker_core_pin_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn capture_ok_and_revalidate_clean() {
        let dir = temp_dir("capture_ok");
        let path = write_file(&dir, "data.txt", b"hello\n");
        let pinned = PinnedFile::capture(&path, 0).unwrap();
        assert_eq!(pinned.path(), path.as_path());
        assert_eq!(pinned.identity().size(), 6);
        assert!(pinned.revalidate().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_after_capture_reports_truncated() {
        let dir = temp_dir("truncate");
        let path = write_file(&dir, "data.txt", b"aaa\nbbb\nccc\n");
        let pinned = PinnedFile::capture(&path, 0).unwrap();
        std::fs::write(&path, b"short\n").unwrap();
        let err = pinned.revalidate().unwrap_err();
        assert_eq!(err.kind(), PinMismatchKind::Truncated);
        assert!(err.detail().contains("size changed"));
        let io_err = err.to_io_error();
        assert_eq!(io_err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unix-only: `dev/ino` reliably trips on rename-over even when the
    /// replacement carries identical size and mtime (the atomic-deploy
    /// hazard). Windows/other targets have no stable key, so the same
    /// swap is undecidable there (see module docs and the Windows test
    /// below).
    #[cfg(unix)]
    #[test]
    fn replace_by_rename_reports_replaced() {
        let dir = temp_dir("replace");
        let path = write_file(&dir, "data.txt", b"original-content\n");
        let pinned = PinnedFile::capture(&path, 0).unwrap();
        let tmp = dir.join("replacement.tmp");
        std::fs::write(&tmp, b"original-content\n").unwrap();
        // Ensure mtime matches to force the identity class to decide.
        // Copy the pinned mtime onto the replacement when available so a
        // same-size/same-mtime rename still trips Replaced via dev/ino.
        if let Some(mtime) = pinned.identity().mtime() {
            let f = File::options().write(true).open(&tmp).unwrap();
            let _ = f.set_modified(mtime);
        }
        std::fs::rename(&tmp, &path).unwrap();
        let err = pinned.revalidate().unwrap_err();
        assert_eq!(err.kind(), PinMismatchKind::Replaced);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows/other targets: no stable file key, so rename-over is
    /// exercised with a size change, which deterministically reports
    /// `Truncated` via the size class. A same-size/same-mtime swap is
    /// undecidable on these targets by design (see module docs).
    #[cfg(not(unix))]
    #[test]
    fn replace_by_rename_with_size_change_reports_truncated() {
        let dir = temp_dir("replace");
        let path = write_file(&dir, "data.txt", b"original-content\n");
        let pinned = PinnedFile::capture(&path, 0).unwrap();
        let tmp = dir.join("replacement.tmp");
        std::fs::write(&tmp, b"replaced-with-different-size\n").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        let err = pinned.revalidate().unwrap_err();
        assert_eq!(err.kind(), PinMismatchKind::Truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_touch_reports_mtime_jump() {
        let dir = temp_dir("mtime");
        let path = write_file(&dir, "data.txt", b"same-size-content!");
        let pinned = PinnedFile::capture(&path, 0).unwrap();
        // Set mtime to a fixed distinct value (no timer-granularity flake).
        let distinct = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        // Avoid colliding with the captured mtime.
        let target = match pinned.identity().mtime() {
            Some(t) if t == distinct => distinct + Duration::from_secs(1),
            _ => distinct,
        };
        let f = File::options().write(true).open(&path).unwrap();
        f.set_modified(target).unwrap();
        drop(f);
        // Size and identity are unchanged, so only the mtime class fires.
        let err = pinned.revalidate().unwrap_err();
        assert_eq!(err.kind(), PinMismatchKind::MtimeJump);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_flags_rejected() {
        let dir = temp_dir("flags");
        let path = write_file(&dir, "data.txt", b"data\n");
        let err = PinnedFile::capture(&path, 1 << 30).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flags_zero_means_all_and_subset_masks_classes() {
        let dir = temp_dir("subset");
        let path = write_file(&dir, "data.txt", b"12345678");
        // PIN_SIZE only: mtime-only change must pass.
        let size_only = PinnedFile::capture(&path, PIN_SIZE).unwrap();
        let distinct = UNIX_EPOCH + Duration::from_secs(1_500_000_001);
        let target = match size_only.identity().mtime() {
            Some(t) if t == distinct => distinct + Duration::from_secs(7),
            _ => distinct,
        };
        {
            let f = File::options().write(true).open(&path).unwrap();
            f.set_modified(target).unwrap();
        }
        assert!(size_only.revalidate().is_ok());
        // PIN_MTIME only: size change must pass.
        let mtime_only = PinnedFile::capture(&path, PIN_MTIME).unwrap();
        std::fs::write(&path, b"12345678-extra-bytes").unwrap();
        // Restore the mtime so only the size class differs (which is masked).
        if let Some(mtime) = mtime_only.identity().mtime() {
            let f = File::options().write(true).open(&path).unwrap();
            let _ = f.set_modified(mtime);
        }
        assert!(mtime_only.revalidate().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_reports_metadata_unavailable() {
        let dir = temp_dir("missing");
        let path = write_file(&dir, "data.txt", b"data\n");
        let pinned = PinnedFile::capture(&path, 0).unwrap();
        std::fs::remove_file(&path).unwrap();
        let err = pinned.revalidate().unwrap_err();
        assert_eq!(err.kind(), PinMismatchKind::MetadataUnavailable);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
