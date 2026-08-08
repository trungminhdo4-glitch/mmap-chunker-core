#ifndef MMAP_CHUNKER_H
#define MMAP_CHUNKER_H

#ifdef __cplusplus
extern "C" {
#endif

#include <stddef.h>
#include <stdint.h>

/* ── Opaque engine handle ─────────────────────────────────────────────────── */

typedef struct CEngineHandle CEngineHandle;

/* ── Chunk view (zero-copy) ───────────────────────────────────────────────── */

typedef struct {
    const uint8_t *data;
    size_t len;
} CChunkView;

/* ── ABI version ──────────────────────────────────────────────────────────── */

#define MMAP_ENGINE_ABI_VERSION 0x00010002U

/* ── Capability bits ──────────────────────────────────────────────────────── */

#define MMAP_ENGINE_CAP_ZERO_COPY              (1U << 0)
#define MMAP_ENGINE_CAP_CONFIGURABLE_DELIMITER (1U << 1)
#define MMAP_ENGINE_CAP_ERROR_STRINGS          (1U << 2)
#define MMAP_ENGINE_CAP_FIXED_SIZE_CHUNKING    (1U << 3)
#define MMAP_ENGINE_CAP_RECORD_PARTITIONING    (1U << 4)

/* ── ABI discovery ────────────────────────────────────────────────────────── */

/**
 * Return the ABI version as (major << 16) | minor.
 *
 * Current: 0x00010002 (v1.2). Always succeeds, never panics.
 * Call once at library load time to verify compatibility.
 */
uint32_t mmap_engine_abi_version(void);

/**
 * Return a bitmask of supported capabilities.
 *
 * Bit 0: ZERO_COPY              — chunk views reference mapped memory directly
 * Bit 1: CONFIGURABLE_DELIMITER — mmap_engine_scan_chunks_ex() available
 * Bit 2: ERROR_STRINGS          — mmap_engine_last_error() returns diagnostic text
 * Bit 3: FIXED_SIZE_CHUNKING    — mmap_engine_scan_fixed() available
 * Bit 4: RECORD_PARTITIONING    — mmap_engine_partition_records() available
 *
 * Call once at library load time to discover which optional features
 * the loaded library provides.
 */
uint32_t mmap_engine_capabilities(void);

/**
 * Return a pointer to the last error message for the calling thread,
 * or NULL if no error occurred.
 *
 * The returned pointer references an internal thread-local buffer
 * (max 255 chars + NUL). It remains valid until the next call to any
 * API function on the same thread. The caller must copy the string
 * if it needs to persist beyond the next API call.
 *
 * Threading: Thread-safe — each thread has its own error buffer.
 */
const char *mmap_engine_last_error(void);

/* ── API functions ────────────────────────────────────────────────────────── */

/**
 * Open and memory-map a file for chunked access.
 *
 * @param path  Null-terminated UTF-8 file path.
 * @return      Opaque engine handle on success, or NULL on failure.
 *              Must be freed with mmap_engine_free().
 *              On failure, call mmap_engine_last_error() for diagnostics.
 *
 * Threading: Must be called from a single thread.
 *
 * Platform notes:
 * - On Windows, `path` must be valid UTF-8. Non-ASCII characters are
 *   supported via UTF-8 to UTF-16 conversion internally.
 * - On POSIX, `path` is passed directly to open(2).
 */
CEngineHandle *mmap_engine_open(const char *path);

/**
 * Scan the mapped file for chunk boundaries using newline ('\\n', 0x0A)
 * as the delimiter.
 *
 * This is the original v1.0 API preserved for backward compatibility.
 * New consumers should prefer mmap_engine_scan_chunks_ex() which
 * supports configurable delimiters.
 *
 * Calling this function replaces any previously computed chunk boundaries.
 *
 * @param handle            Valid handle from mmap_engine_open().
 * @param chunk_size_bytes  Approximate chunk size in bytes (minimum 1;
 *                          values of 0 are silently clamped to 1).
 * @return                  Number of chunks found, or 0 on error / empty file.
 *                          On error, call mmap_engine_last_error() for diagnostics.
 *
 * Threading: Must be called from a single thread. Concurrent calls to
 * mmap_engine_get_chunk() are permitted ONLY after this function returns.
 */
size_t mmap_engine_scan_chunks(CEngineHandle *handle, size_t chunk_size_bytes);

/**
 * Scan the mapped file for chunk boundaries with a configurable delimiter.
 *
 * Chunks are created at approximately `chunk_size_bytes` intervals.
 * Each chunk boundary is placed immediately after a `delimiter` byte
 * found at or after the target offset. The last chunk extends to the
 * end of the file.
 *
 * Common delimiter values:
 *   '\\n' (0x0A) — newline (JSONL, NDJSON, logs)
 *   ','  (0x2C) — comma (CSV)
 *   '\\t' (0x09) — tab (TSV)
 *   '|'  (0x7C) — pipe
 *   '\\0' (0x00) — NUL byte (binary framing)
 *
 * Calling this function replaces any previously computed chunk boundaries.
 *
 * @param handle            Valid handle from mmap_engine_open().
 * @param chunk_size_bytes  Approximate chunk size in bytes (minimum 1;
 *                          values of 0 are silently clamped to 1).
 * @param delimiter         Byte value used to detect record boundaries.
 * @return                  Number of chunks found, or 0 on error / empty file.
 *                          On error, call mmap_engine_last_error() for diagnostics.
 *
 * Threading: Same contract as mmap_engine_scan_chunks().
 *
 * Added in ABI v1.0 (detect with MMAP_ENGINE_CAP_CONFIGURABLE_DELIMITER).
 */
size_t mmap_engine_scan_chunks_ex(CEngineHandle *handle,
                                  size_t chunk_size_bytes,
                                  uint8_t delimiter);

/**
 * Scan the mapped file into sequential fixed-size chunks.
 *
 * Chunks are created at exact `chunk_size_bytes` intervals, with the
 * last chunk potentially shorter at EOF. No delimiter semantics — this
 * mode is suitable for binary/non-record workloads.
 *
 * Chunk i covers [i*size, min((i+1)*size, file_len)).
 * All non-final chunks have length exactly `chunk_size_bytes`.
 * The final chunk has length `file_len % chunk_size_bytes`, or a full
 * chunk when the file size divides evenly.
 *
 * `chunk_size_bytes` of 0 is silently clamped to 1 (consistent with
 * all other scan functions — see mmap_engine_scan_chunks()).
 *
 * Calling this function replaces any previously computed chunk boundaries.
 * The most recent scan determines the layout returned by
 * mmap_engine_get_chunk().
 *
 * @param handle            Valid handle from mmap_engine_open().
 * @param chunk_size_bytes  Exact chunk size in bytes (minimum 1;
 *                          values of 0 are silently clamped to 1).
 * @return                  Number of chunks found, or 0 on error / empty file.
 *                          On error, call mmap_engine_last_error() for diagnostics.
 *
 * Threading: Same contract as mmap_engine_scan_chunks().
 *
 * Added in ABI v1.1 (detect with MMAP_ENGINE_CAP_FIXED_SIZE_CHUNKING).
 */
size_t mmap_engine_scan_fixed(CEngineHandle *handle, size_t chunk_size_bytes);

/**
 * Plan record-aligned partition byte ranges for N-way parallel consumers.
 *
 * Computes approximately balanced, record-aligned byte ranges that partition
 * the mapped file into `requested_partitions` contiguous, non-overlapping
 * segments. Each segment boundary falls immediately after a `delimiter` byte,
 * ensuring no record is ever split across partition boundaries.
 *
 * How it works:
 *   For each boundary i = 1..N-1, the ideal absolute target position
 *   `floor(file_len * i / N)` is computed independently, then a forward
 *   search from that position locates the next delimiter. Each boundary
 *   is placed immediately after that delimiter byte.
 *
 *   This "absolute target" strategy prevents cumulative drift that
 *   iterative (previous-boundary + chunk_size) approaches suffer from.
 *
 * Key properties:
 *   - Complete coverage: first.start == 0, last.end == file_len
 *   - No gaps, no overlaps — contiguous byte ranges
 *   - Record integrity: non-final partitions always end after delimiter
 *   - Deterministic: same file + parameters always produce same result
 *   - O(partitions) metadata, bounded scanning
 *   - No full-file sequential scan required
 *
 * Edge cases:
 *   - Zero partitions requested: returns 0, error set
 *   - One partition: returns 1 (entire file)
 *   - No delimiter anywhere in file: returns 1 (entire file)
 *   - Giant record spanning multiple ideal targets: boundaries collapse,
 *     effective partition count < requested count
 *   - Empty file: returns 0 (no partitions)
 *
 * Calling this function replaces any previously computed chunk boundaries
 * (delimited, fixed, or prior partition plan). The most recent plan call
 * determines the layout returned by mmap_engine_get_chunk().
 *
 * @param handle                Valid handle from mmap_engine_open().
 * @param requested_partitions  Desired number of partitions (must be > 0).
 * @param delimiter             Byte value used to detect record boundaries.
 * @return                      Actual partition count (may be < requested),
 *                              or 0 on error / empty file.
 *                              On error, call mmap_engine_last_error().
 *
 * Threading: Same contract as mmap_engine_scan_chunks().
 *
 * Added in ABI v1.2 (detect with MMAP_ENGINE_CAP_RECORD_PARTITIONING).
 */
size_t mmap_engine_partition_records(CEngineHandle *handle,
                                     size_t requested_partitions,
                                     uint8_t delimiter);

/**
 * Retrieve a chunk view by index (zero-copy).
 *
 * The `data` pointer references the memory-mapped file directly and
 * remains valid until mmap_engine_free() is called. Writing to or beyond
 * `data[len]` is undefined behavior.
 *
 * @param handle     Valid handle from mmap_engine_open().
 * @param index      Zero-based chunk index (< chunk_count).
 * @param out_chunk  Output parameter filled with chunk data and length.
 * @return           0 on success, -1 on error (null pointer or out of bounds).
 *                   On error, call mmap_engine_last_error() for diagnostics.
 *
 * Threading: Safe to call from multiple threads concurrently after
 * mmap_engine_scan_chunks() has returned, provided the handle is not
 * being freed or re-scanned.
 */
int32_t mmap_engine_get_chunk(CEngineHandle *handle, size_t index,
                              CChunkView *out_chunk);

/**
 * Free the engine handle and release all resources (file mapping, OS
 * handles, chunk metadata).
 *
 * After this call the handle is invalid and all chunk views obtained
 * from mmap_engine_get_chunk() must no longer be used.
 *
 * @param handle  Valid handle from mmap_engine_open(), or NULL (no-op).
 *
 * Threading: Must be called from a single thread. No other operations
 * may be in flight when this is called.
 */
void mmap_engine_free(CEngineHandle *handle);

/* ── File mutation contract ───────────────────────────────────────────────── */
/*
 * The engine provides a read-only view of the file at mapping time.
 *
 * POSIX (Linux, macOS):
 *   If another process truncates or overwrites the file after the mapping
 *   is established, accessing the affected pages may deliver SIGBUS or
 *   return zero-filled pages, depending on the kernel. The engine does
 *   NOT detect or recover from this.
 *
 * Windows:
 *   If the file is truncated after the mapping, the mapped view may
 *   become invalid and access may cause an access violation. Microsoft
 *   documents that the behavior is undefined in this case.
 *
 * RECOMMENDATION:
 *   The caller should treat the input file as immutable for the lifetime
 *   of the engine handle. If file mutation is possible, use a copy or
 *   snapshot instead of memory-mapping the live file.
 */

/* ── ABI stability ────────────────────────────────────────────────────────── */
/*
 * Versioning:
 *   - mmap_engine_abi_version() returns (major << 16) | minor.
 *   - PATCH releases (bug fixes) do not change the ABI version.
 *   - MINOR releases add new functions without changing existing signatures.
 *   - MAJOR releases may break ABI compatibility.
 *
 * History:
 *   v1.0 (0x00010000): Initial release — 8 core functions.
 *   v1.1 (0x00010001): Added mmap_engine_scan_fixed() + CAP_FIXED_SIZE_CHUNKING.
 *   v1.2 (0x00010002): Added mmap_engine_partition_records() + CAP_RECORD_PARTITIONING.
 *
 * CChunkView layout (guaranteed by #[repr(C)]):
 *
 *   offset  field   type            size (64-bit)
 *   ------  -----   ----            -------------
 *   0       data    const uint8_t*  8
 *   8       len     size_t          8
 *   total: 16 bytes
 *
 * CEngineHandle is an opaque pointer type. The caller must never
 * dereference or sizeof() it.
 */

#ifdef __cplusplus
}
#endif

#endif /* MMAP_CHUNKER_H */
