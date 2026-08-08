#include "../mmap_chunker.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

static int failures = 0;
static int tests = 0;

#define TEST(name) do { tests++; } while(0)
#define CHECK(cond, msg) do { \
    if (!(cond)) { \
        failures++; \
        fprintf(stderr, "FAIL [%d]: %s\n", __LINE__, msg); \
    } \
} while(0)

static void write_test_file(const char *path, const uint8_t *data, size_t len) {
    FILE *f = fopen(path, "wb");
    if (!f) { perror("fopen"); exit(1); }
    fwrite(data, 1, len, f);
    fclose(f);
}

int main(void) {
    /* ── 1. ABI version ──────────────────────────────────────────── */
    TEST("abi_version");
    CHECK(mmap_engine_abi_version() == 0x00010001U, "ABI version must be 0x00010001");

    /* ── 2. Capabilities ─────────────────────────────────────────── */
    TEST("capabilities");
    uint32_t caps = mmap_engine_capabilities();
    CHECK(caps & (1U << 0), "must have ZERO_COPY");
    CHECK(caps & (1U << 1), "must have CONFIGURABLE_DELIMITER");
    CHECK(caps & (1U << 2), "must have ERROR_STRINGS");
    CHECK(caps & (1U << 3), "must have FIXED_SIZE_CHUNKING");

    /* ── 3. Error on NULL path ───────────────────────────────────── */
    TEST("open_null");
    CHECK(mmap_engine_open(NULL) == NULL, "open(NULL) must return NULL");
    const char *err = mmap_engine_last_error();
    CHECK(err != NULL && strstr(err, "null") != NULL, "error must mention null");

    /* ── 4. Error on nonexistent file ────────────────────────────── */
    TEST("open_nonexistent");
    CHECK(mmap_engine_open("/no/such/file") == NULL, "nonexistent must return NULL");
    CHECK(mmap_engine_last_error() != NULL, "must have error");

    /* ── 5. Error cleared on success ─────────────────────────────── */
    TEST("error_cleared");
    const char *test_path = "c_consumer_test.dat";
    const uint8_t content[] = "line1,data1\nline2,data2\nline3,data3\n";
    size_t content_len = sizeof(content) - 1;
    write_test_file(test_path, content, content_len);

    CEngineHandle *h = mmap_engine_open(test_path);
    CHECK(h != NULL, "valid file must return handle");
    CHECK(mmap_engine_last_error() == NULL, "error must be cleared after success");

    /* ── 6. scan_chunks (newline) ─────────────────────────────────── */
    TEST("scan_chunks");
    size_t count = mmap_engine_scan_chunks(h, 32);
    CHECK(count > 0, "must find chunks with newline delimiter");

    /* ── 7. Validate all chunks (coverage) ────────────────────────── */
    TEST("chunk_coverage");
    size_t total = 0;
    for (size_t i = 0; i < count; i++) {
        CChunkView view;
        int ret = mmap_engine_get_chunk(h, i, &view);
        CHECK(ret == 0, "get_chunk must succeed for valid index");
        CHECK(view.data != NULL, "chunk data must not be NULL");
        CHECK(view.len > 0, "chunk must have positive length");
        total += view.len;
    }
    CHECK(total == content_len, "total chunk bytes must equal file size");

    /* ── 8. Out-of-bounds index → error ──────────────────────────── */
    TEST("oob_index");
    CChunkView view;
    int ret = mmap_engine_get_chunk(h, count, &view);
    CHECK(ret == -1, "OOB index must return -1");
    err = mmap_engine_last_error();
    CHECK(err != NULL, "must have error after OOB");
    CHECK(strstr(err, "out of bounds") != NULL, "error must mention bounds");

    /* ── 9. NULL out pointer → error ─────────────────────────────── */
    TEST("null_out");
    ret = mmap_engine_get_chunk(h, 0, NULL);
    CHECK(ret == -1, "NULL out pointer must return -1");
    CHECK(mmap_engine_last_error() != NULL, "must have error");

    /* ── 10. chunk_size=0 clamped to 1 ───────────────────────────── */
    TEST("chunk_size_zero");
    CEngineHandle *h2 = mmap_engine_open(test_path);
    CHECK(h2 != NULL, "must open");
    CHECK(mmap_engine_scan_chunks(h2, 0) > 0, "chunk_size=0 must yield >=1 chunk");
    mmap_engine_free(h2);

    /* ── 11. scan_chunks_ex with comma delimiter ──────────────────── */
    TEST("scan_comma");
    h2 = mmap_engine_open(test_path);
    CHECK(h2 != NULL, "must open");
    size_t comma_count = mmap_engine_scan_chunks_ex(h2, 1, ',');
    CHECK(comma_count > 0, "comma delimiter must find chunks");
    size_t comma_total = 0;
    for (size_t i = 0; i < comma_count; i++) {
        CChunkView v;
        CHECK(mmap_engine_get_chunk(h2, i, &v) == 0, "get chunk");
        comma_total += v.len;
    }
    CHECK(comma_total == content_len, "comma-scan coverage must match file size");
    mmap_engine_free(h2);

    /* ── 12. scan_chunks == scan_chunks_ex('\\n') ─────────────────── */
    TEST("scan_compat");
    CEngineHandle *ha = mmap_engine_open(test_path);
    CEngineHandle *hb = mmap_engine_open(test_path);
    CHECK(ha != NULL && hb != NULL, "must open");
    size_t ca = mmap_engine_scan_chunks(ha, 32);
    size_t cb = mmap_engine_scan_chunks_ex(hb, 32, '\n');
    CHECK(ca == cb, "scan_chunks and scan_chunks_ex(\\n) must match");
    for (size_t i = 0; i < ca; i++) {
        CChunkView va, vb;
        mmap_engine_get_chunk(ha, i, &va);
        mmap_engine_get_chunk(hb, i, &vb);
        CHECK(va.len == vb.len, "lengths must match");
        CHECK(memcmp(va.data, vb.data, va.len) == 0, "content must match");
    }
    mmap_engine_free(ha);
    mmap_engine_free(hb);

    /* ── 13. Empty file → 0 chunks, handle valid ──────────────────── */
    TEST("empty_file");
    write_test_file("c_consumer_test_empty.dat", (const uint8_t *)"", 0);
    CEngineHandle *he = mmap_engine_open("c_consumer_test_empty.dat");
    CHECK(he != NULL, "empty file must return valid handle");
    CHECK(mmap_engine_scan_chunks(he, 1024) == 0, "empty file must have 0 chunks");
    mmap_engine_free(he);
    remove("c_consumer_test_empty.dat");

    /* ── 14. Free main handle ─────────────────────────────────────── */
    TEST("free");
    mmap_engine_free(h);

    /* ── 15. struct layout ────────────────────────────────────────── */
    TEST("struct_layout");
    CHECK(sizeof(CChunkView) >= sizeof(void *) + sizeof(size_t),
          "CChunkView must hold pointer + size");

    /* ── 16. Fixed-size: exact split ──────────────────────────────── */
    TEST("fixed_exact");
    const uint8_t fixed_data[] = "AAAABBBBCCCCDDDDEEEEFFFF";
    size_t fixed_len = sizeof(fixed_data) - 1;
    write_test_file("c_consumer_fixed_test.dat", fixed_data, fixed_len);
    CEngineHandle *hf = mmap_engine_open("c_consumer_fixed_test.dat");
    CHECK(hf != NULL, "must open fixed test file");
    size_t fc = mmap_engine_scan_fixed(hf, 4);
    CHECK(fc == 6, "24B/4B = 6 chunks");
    CChunkView fv;
    size_t ftotal = 0;
    for (size_t i = 0; i < fc; i++) {
        CHECK(mmap_engine_get_chunk(hf, i, &fv) == 0, "get chunk");
        CHECK(fv.len == 4, "each chunk must be exactly 4 bytes");
        ftotal += fv.len;
    }
    CHECK(ftotal == fixed_len, "total must equal file size");
    mmap_engine_free(hf);
    remove("c_consumer_fixed_test.dat");

    /* ── 17. Fixed-size: short last chunk ─────────────────────────── */
    TEST("fixed_short_last");
    write_test_file("c_consumer_fixed_short.dat",
                    (const uint8_t *)"XXXXXXXXX", 9);
    CEngineHandle *hfs = mmap_engine_open("c_consumer_fixed_short.dat");
    CHECK(hfs != NULL, "must open");
    size_t fcs = mmap_engine_scan_fixed(hfs, 4);
    CHECK(fcs == 3, "9B/4B = 3 chunks");
    CChunkView fsv;
    CHECK(mmap_engine_get_chunk(hfs, 0, &fsv) == 0, "");
    CHECK(fsv.len == 4, "");
    CHECK(mmap_engine_get_chunk(hfs, 1, &fsv) == 0, "");
    CHECK(fsv.len == 4, "");
    CHECK(mmap_engine_get_chunk(hfs, 2, &fsv) == 0, "");
    CHECK(fsv.len == 1, "last chunk must be 1 byte");
    mmap_engine_free(hfs);
    remove("c_consumer_fixed_short.dat");

    /* ── 18. Fixed-size: size larger than file ────────────────────── */
    TEST("fixed_size_larger");
    write_test_file("c_consumer_fixed_large.dat",
                    (const uint8_t *)"tiny", 4);
    CEngineHandle *hfl = mmap_engine_open("c_consumer_fixed_large.dat");
    CHECK(hfl != NULL, "must open");
    CHECK(mmap_engine_scan_fixed(hfl, 1024) == 1, "1 chunk");
    CChunkView flv;
    CHECK(mmap_engine_get_chunk(hfl, 0, &flv) == 0, "");
    CHECK(flv.len == 4, "single chunk covers whole file");
    mmap_engine_free(hfl);
    remove("c_consumer_fixed_large.dat");

    /* ── 19. Fixed-size: chunk_size=0 clamped ─────────────────────── */
    TEST("fixed_size_zero");
    write_test_file("c_consumer_fixed_zero.dat",
                    (const uint8_t *)"abc", 3);
    CEngineHandle *hfz = mmap_engine_open("c_consumer_fixed_zero.dat");
    CHECK(hfz != NULL, "must open");
    CHECK(mmap_engine_scan_fixed(hfz, 0) == 3, "0 clamps to 1 → 3 chunks");
    CChunkView fzv;
    CHECK(mmap_engine_get_chunk(hfz, 0, &fzv) == 0, "");
    CHECK(fzv.len == 1, "");
    CHECK(fzv.data[0] == 'a', "");
    mmap_engine_free(hfz);
    remove("c_consumer_fixed_zero.dat");

    /* ── 20. Fixed-size: empty file ───────────────────────────────── */
    TEST("fixed_empty");
    write_test_file("c_consumer_fixed_empty.dat", (const uint8_t *)"", 0);
    CEngineHandle *hfe = mmap_engine_open("c_consumer_fixed_empty.dat");
    CHECK(hfe != NULL, "empty file must return valid handle");
    CHECK(mmap_engine_scan_fixed(hfe, 256) == 0, "empty file → 0 chunks");
    mmap_engine_free(hfe);
    remove("c_consumer_fixed_empty.dat");

    /* ── 21. Fixed-size: no delimiter alignment ───────────────────── */
    TEST("fixed_no_delim");
    write_test_file("c_consumer_fixed_nodelim.dat",
                    (const uint8_t *)"ab\ncd\nef", 8);
    CEngineHandle *hfd = mmap_engine_open("c_consumer_fixed_nodelim.dat");
    CHECK(hfd != NULL, "must open");
    CHECK(mmap_engine_scan_fixed(hfd, 3) == 3, "8B/3B = 3 chunks");
    CChunkView fdv;
    CHECK(mmap_engine_get_chunk(hfd, 0, &fdv) == 0, "");
    CHECK(fdv.len == 3, "first chunk exactly 3B despite newline");
    CHECK(mmap_engine_get_chunk(hfd, 1, &fdv) == 0, "");
    CHECK(fdv.len == 3, "second chunk exactly 3B");
    CHECK(mmap_engine_get_chunk(hfd, 2, &fdv) == 0, "");
    CHECK(fdv.len == 2, "last chunk 2B");
    mmap_engine_free(hfd);
    remove("c_consumer_fixed_nodelim.dat");

    /* ── 22. Mode switching: delimited → fixed → delimited ────────── */
    TEST("mode_switch");
    write_test_file("c_consumer_mode_switch.dat",
                    (const uint8_t *)"aaa\nbbb\nccc\nddd\n", 16);
    CEngineHandle *hms = mmap_engine_open("c_consumer_mode_switch.dat");
    CHECK(hms != NULL, "must open");
    size_t ms_dc = mmap_engine_scan_chunks_ex(hms, 4, '\n');
    CHECK(ms_dc > 0, "delimited scan");
    CChunkView msv;
    CHECK(mmap_engine_get_chunk(hms, 0, &msv) == 0, "");
    /* delimited mode: step=4, finds \n at offset7 → chunk(0,8) */
    CHECK(msv.len == 8, "delimited chunk 0 must be 8B");
    size_t ms_fc = mmap_engine_scan_fixed(hms, 4);
    CHECK(ms_fc == 4, "fixed(4) → 4 chunks");
    CHECK(mmap_engine_get_chunk(hms, 0, &msv) == 0, "");
    CHECK(msv.len == 4, "fixed chunk 0 must be 4B");
    size_t ms_dc2 = mmap_engine_scan_chunks_ex(hms, 4, '\n');
    CHECK(ms_dc2 == ms_dc, "back to delimited must match");
    mmap_engine_free(hms);
    remove("c_consumer_mode_switch.dat");

    /* ── Cleanup ──────────────────────────────────────────────────── */
    remove(test_path);

    /* ── Report ───────────────────────────────────────────────────── */
    if (failures == 0) {
        printf("PASS: all %d C ABI consumer tests passed\n", tests);
        return 0;
    }
    fprintf(stderr, "FAIL: %d/%d tests failed\n", failures, tests);
    return 1;
}
