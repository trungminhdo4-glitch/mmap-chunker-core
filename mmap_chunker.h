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

/* ── API functions ────────────────────────────────────────────────────────── */

/**
 * Open and memory-map a file for chunked access.
 *
 * @param path  Null-terminated UTF-8 file path.
 * @return      Opaque engine handle on success, or NULL on failure.
 *              Must be freed with mmap_engine_free().
 *
 * Threading: Must be called from a single thread. The returned handle is not
 * safe to use from multiple threads concurrently.
 *
 * Platform notes:
 * - On Windows, `path` must be valid UTF-8. Non-ASCII characters are
 *   supported via UTF-8 to UTF-16 conversion internally.
 * - On POSIX, `path` is passed directly to open(2).
 */
CEngineHandle *mmap_engine_open(const char *path);

/**
 * Scan the mapped file for chunk boundaries.
 *
 * Chunks are created at approximately `chunk_size_bytes` intervals.
 * Each chunk boundary is placed immediately after a newline (`\n`, 0x0A)
 * found at or after the target offset.
 *
 * Calling this function replaces any previously computed chunk boundaries.
 *
 * @param handle            Valid handle from mmap_engine_open().
 * @param chunk_size_bytes  Approximate chunk size in bytes (minimum 1;
 *                          values of 0 are silently clamped to 1).
 * @return                  Number of chunks found, or 0 on error / empty file.
 *
 * Threading: Must be called from a single thread. Concurrent calls to
 * mmap_engine_get_chunk() are permitted ONLY after this function returns.
 */
size_t mmap_engine_scan_chunks(CEngineHandle *handle, size_t chunk_size_bytes);

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
