#include "mmap_chunker.h"

#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const uint64_t FNV_OFFSET = UINT64_C(14695981039346656037);
static const uint64_t FNV_PRIME = UINT64_C(1099511628211);

static void fail(const char *message) {
    fprintf(stderr, "FAIL: %s\n", message);
    exit(EXIT_FAILURE);
}

static uint8_t *read_file(const char *path, size_t *length) {
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        fail("could not open fixture");
    }
    if (fseek(file, 0, SEEK_END) != 0) {
        fail("could not seek fixture");
    }
    long end = ftell(file);
    if (end < 0) {
        fail("could not measure fixture");
    }
    if (fseek(file, 0, SEEK_SET) != 0) {
        fail("could not rewind fixture");
    }
    *length = (size_t)end;
    uint8_t *data = (uint8_t *)malloc(*length == 0 ? 1 : *length);
    if (data == NULL) {
        fail("could not allocate fixture");
    }
    if (*length != 0 && fread(data, 1, *length, file) != *length) {
        fail("could not read fixture");
    }
    fclose(file);
    return data;
}

static uint64_t hash_bytes(const uint8_t *data, size_t length,
                           uint64_t hash) {
    for (size_t i = 0; i < length; ++i) {
        hash ^= data[i];
        hash *= FNV_PRIME;
    }
    return hash;
}

static size_t capture_plan(CEngineHandle *handle, size_t *lengths,
                           size_t capacity, uint64_t *hash,
                           const uint8_t *source, size_t source_length) {
    size_t count = mmap_engine_partition_records(handle, 4, '\n');
    if (count == 0 || count > capacity) {
        fail("unexpected partition count");
    }
    size_t offset = 0;
    *hash = FNV_OFFSET;
    for (size_t index = 0; index < count; ++index) {
        CChunkView view = {0};
        if (mmap_engine_get_chunk(handle, index, &view) != 0) {
            fail("could not retrieve partition");
        }
        if (view.data == NULL || view.len == 0 ||
            view.len > source_length - offset) {
            fail("invalid partition view");
        }
        if (memcmp(view.data, source + offset, view.len) != 0) {
            fail("partition bytes differ from fixture");
        }
        if (index + 1 < count && view.data[view.len - 1] != '\n') {
            fail("non-final partition splits a record");
        }
        lengths[index] = view.len;
        *hash = hash_bytes(view.data, view.len, *hash);
        offset += view.len;
    }
    if (offset != source_length) {
        fail("partition plan does not reconstruct the fixture");
    }
    return count;
}

static void write_and_compare(const char *expected_path, const char *output_path,
                              const char *result) {
    FILE *expected = fopen(expected_path, "rb");
    if (expected == NULL) {
        fail("could not open expected result");
    }
    char expected_line[2048] = {0};
    if (fgets(expected_line, sizeof(expected_line), expected) == NULL) {
        fail("could not read expected result");
    }
    fclose(expected);
    expected_line[strcspn(expected_line, "\r\n")] = '\0';
    if (strcmp(expected_line, result) != 0) {
        fprintf(stderr, "FAIL: canonical result mismatch\nexpected: %s\nactual:   %s\n",
                expected_line, result);
        exit(EXIT_FAILURE);
    }

    FILE *output = fopen(output_path, "wb");
    if (output == NULL || fputs(result, output) < 0 || fputc('\n', output) == EOF) {
        fail("could not write result");
    }
    fclose(output);
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: c_consumer FIXTURE EXPECTED OUTPUT\n");
        return EXIT_FAILURE;
    }

    if (mmap_engine_abi_version() != MMAP_ENGINE_ABI_VERSION ||
        mmap_engine_capabilities() != 63U) {
        fail("ABI discovery mismatch");
    }
    if (sizeof(CChunkView) != 16 || offsetof(CChunkView, data) != 0 ||
        offsetof(CChunkView, len) != 8) {
        fail("CChunkView layout mismatch");
    }

    size_t source_length = 0;
    uint8_t *source = read_file(argv[1], &source_length);
    CEngineHandle *handle = mmap_engine_open(argv[1]);
    if (handle == NULL) {
        fail("mmap_engine_open failed for UTF-8 path");
    }

    size_t lengths[16] = {0};
    size_t repeat_lengths[16] = {0};
    uint64_t hash = 0;
    uint64_t repeat_hash = 0;
    size_t count = capture_plan(handle, lengths, 16, &hash, source, source_length);
    size_t repeat_count = capture_plan(handle, repeat_lengths, 16, &repeat_hash,
                                       source, source_length);
    if (count != repeat_count || hash != repeat_hash ||
        memcmp(lengths, repeat_lengths, count * sizeof(size_t)) != 0) {
        fail("partition plan is not deterministic");
    }

    if (mmap_engine_partition_records(handle, 0, '\n') != 0) {
        fail("N=0 unexpectedly succeeded");
    }
    const char *error = mmap_engine_last_error();
    if (error == NULL || strcmp(error, "requested_partitions must be > 0") != 0) {
        fail("N=0 error contract mismatch");
    }
    char error_copy[256];
    snprintf(error_copy, sizeof(error_copy), "%s", error);
    size_t newline_count = 0;
    for (size_t i = 0; i < source_length; ++i) {
        newline_count += source[i] == '\n';
    }
    size_t record_count = newline_count + (source_length != 0 &&
                                           source[source_length - 1] != '\n');
    mmap_engine_free(handle);
    free(source);
    char lengths_text[256] = {0};
    size_t used = 0;
    for (size_t i = 0; i < count; ++i) {
        int written = snprintf(lengths_text + used, sizeof(lengths_text) - used,
                               "%s%zu", i == 0 ? "" : ",", lengths[i]);
        if (written < 0 || (size_t)written >= sizeof(lengths_text) - used) {
            fail("partition lengths do not fit result");
        }
        used += (size_t)written;
    }

    char result[2048];
    int written = snprintf(
        result, sizeof(result),
        "abi_version=%u;capabilities=%u;partition_count=%zu;partition_lengths=%s;"
        "total_length=%zu;record_count=%zu;fnv1a64=%016llx;deterministic=1;"
        "n0_error=%s;chunk_view_size=%zu;chunk_view_data_offset=%zu;"
        "chunk_view_len_offset=%zu",
        MMAP_ENGINE_ABI_VERSION, 63U, count, lengths_text, source_length,
        record_count, (unsigned long long)hash, error_copy, sizeof(CChunkView),
        offsetof(CChunkView, data), offsetof(CChunkView, len));
    if (written < 0 || (size_t)written >= sizeof(result)) {
        fail("result does not fit buffer");
    }
    write_and_compare(argv[2], argv[3], result);
    puts("PASS: C conformance consumer");
    return EXIT_SUCCESS;
}
