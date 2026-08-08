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
    CHECK(mmap_engine_abi_version() == 0x00010002U, "ABI version must be 0x00010002");

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

    /* ── 23. Partition: basic N=2 ──────────────────────────────────── */
    TEST("partition_n2");
    write_test_file("c_consumer_partition_n2.dat",
                    (const uint8_t *)"rec1\nrec2\nrec3\nrec4\n", 20);
    CEngineHandle *hp2 = mmap_engine_open("c_consumer_partition_n2.dat");
    CHECK(hp2 != NULL, "must open");
    size_t pc2 = mmap_engine_partition_records(hp2, 2, '\n');
    CHECK(pc2 == 2, "4 records, N=2 → 2 partitions");
    CChunkView pv2;
    size_t ptotal2 = 0;
    for (size_t i = 0; i < pc2; i++) {
        CHECK(mmap_engine_get_chunk(hp2, i, &pv2) == 0, "get partition");
        CHECK(pv2.len > 0, "partition must be non-empty");
        ptotal2 += pv2.len;
    }
    CHECK(ptotal2 == 20, "total must equal file size");
    CHECK(mmap_engine_get_chunk(hp2, pc2, &pv2) == -1, "OOB");
    mmap_engine_free(hp2);
    remove("c_consumer_partition_n2.dat");

    /* ── 24. Partition: N=1 ────────────────────────────────────────── */
    TEST("partition_n1");
    write_test_file("c_consumer_partition_n1.dat",
                    (const uint8_t *)"a\nb\n", 4);
    CEngineHandle *hp1 = mmap_engine_open("c_consumer_partition_n1.dat");
    CHECK(hp1 != NULL, "must open");
    size_t pc1 = mmap_engine_partition_records(hp1, 1, '\n');
    CHECK(pc1 == 1, "N=1 → 1 partition");
    CChunkView pv1;
    CHECK(mmap_engine_get_chunk(hp1, 0, &pv1) == 0, "");
    CHECK(pv1.len == 4, "single partition = whole file");
    mmap_engine_free(hp1);
    remove("c_consumer_partition_n1.dat");

    /* ── 25. Partition: N=0 → error ────────────────────────────────── */
    TEST("partition_n0");
    write_test_file("c_consumer_partition_n0.dat",
                    (const uint8_t *)"data\n", 5);
    CEngineHandle *hp0 = mmap_engine_open("c_consumer_partition_n0.dat");
    CHECK(hp0 != NULL, "must open");
    CHECK(mmap_engine_partition_records(hp0, 0, '\n') == 0, "N=0 → error");
    CHECK(mmap_engine_last_error() != NULL, "error must be set");
    mmap_engine_free(hp0);
    remove("c_consumer_partition_n0.dat");

    /* ── 26. Partition: no delimiter → 1 partition ─────────────────── */
    TEST("partition_nodelim");
    write_test_file("c_consumer_partition_nodelim.dat",
                    (const uint8_t *)"no_newlines", 11);
    CEngineHandle *hpnd = mmap_engine_open("c_consumer_partition_nodelim.dat");
    CHECK(hpnd != NULL, "must open");
    size_t pcnd = mmap_engine_partition_records(hpnd, 8, '\n');
    CHECK(pcnd == 1, "no delim → 1 partition");
    CChunkView pvnd;
    CHECK(mmap_engine_get_chunk(hpnd, 0, &pvnd) == 0, "");
    CHECK(pvnd.len == 11, "partition = whole file");
    mmap_engine_free(hpnd);
    remove("c_consumer_partition_nodelim.dat");

    /* ── 27. Partition: empty file → 0 ─────────────────────────────── */
    TEST("partition_empty");
    write_test_file("c_consumer_partition_empty.dat",
                    (const uint8_t *)"", 0);
    CEngineHandle *hpe = mmap_engine_open("c_consumer_partition_empty.dat");
    CHECK(hpe != NULL, "empty file handle");
    CHECK(mmap_engine_partition_records(hpe, 4, '\n') == 0, "empty → 0");
    mmap_engine_free(hpe);
    remove("c_consumer_partition_empty.dat");

    /* ── 28. Partition: null handle → 0 ────────────────────────────── */
    TEST("partition_null");
    CHECK(mmap_engine_partition_records(NULL, 4, '\n') == 0, "null handle → 0");

    /* ── 29. Partition: N > records ────────────────────────────────── */
    TEST("partition_n_gt_records");
    write_test_file("c_consumer_partition_ngt.dat",
                    (const uint8_t *)"a\nb\n", 4);
    CEngineHandle *hpng = mmap_engine_open("c_consumer_partition_ngt.dat");
    CHECK(hpng != NULL, "must open");
    size_t pcng = mmap_engine_partition_records(hpng, 100, '\n');
    CHECK(pcng > 0, "should produce some partitions");
    CHECK(pcng < 100, "should not produce 100 partitions for 2 recs");
    CChunkView pvng;
    for (size_t i = 0; i < pcng; i++) {
        CHECK(mmap_engine_get_chunk(hpng, i, &pvng) == 0, "");
        CHECK(pvng.len > 0, "no empty partitions");
    }
    mmap_engine_free(hpng);
    remove("c_consumer_partition_ngt.dat");

    /* ── 30. Partition mode switch: delimited ↔ partitioned ────────── */
    TEST("partition_mode_switch");
    write_test_file("c_consumer_partition_ms.dat",
                    (const uint8_t *)"line0\nline1\nline2\nline3\nline4\nline5\n", 36);
    CEngineHandle *hpms = mmap_engine_open("c_consumer_partition_ms.dat");
    CHECK(hpms != NULL, "must open");
    CChunkView pmsv;

    /* Delimited first */
    size_t d = mmap_engine_scan_chunks_ex(hpms, 12, '\n');
    CHECK(d > 0, "delimited scan");
    CHECK(mmap_engine_get_chunk(hpms, 0, &pmsv) == 0, "");

    /* Switch to partition */
    size_t p = mmap_engine_partition_records(hpms, 3, '\n');
    CHECK(p == 3, "partition scan");
    size_t ptotalms = 0;
    for (size_t i = 0; i < p; i++) {
        CHECK(mmap_engine_get_chunk(hpms, i, &pmsv) == 0, "");
        ptotalms += pmsv.len;
    }
    CHECK(ptotalms == 36, "coverage");

    /* Switch back to delimited */
    size_t d2 = mmap_engine_scan_chunks_ex(hpms, 12, '\n');
    CHECK(d2 == d, "back to delimited matches");

    mmap_engine_free(hpms);
    remove("c_consumer_partition_ms.dat");

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
