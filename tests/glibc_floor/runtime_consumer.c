#include "mmap_chunker.h"

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: runtime_consumer FIXTURE\n");
        return EXIT_FAILURE;
    }
    if (mmap_engine_abi_version() != MMAP_ENGINE_ABI_VERSION ||
        mmap_engine_capabilities() != 63U) {
        fail("ABI discovery mismatch");
    }

    size_t source_length = 0;
    uint8_t *source = read_file(argv[1], &source_length);
    CEngineHandle *handle = mmap_engine_open(argv[1]);
    if (handle == NULL) {
        fail("mmap_engine_open failed for UTF-8 path");
    }

    size_t count = mmap_engine_partition_records(handle, 4, '\n');
    if (count == 0 || count > 16) {
        fail("unexpected partition count");
    }
    size_t offset = 0;
    for (size_t index = 0; index < count; ++index) {
        CChunkView view = {0};
        if (mmap_engine_get_chunk(handle, index, &view) != 0 ||
            view.data == NULL || view.len == 0 ||
            view.len > source_length - offset) {
            fail("invalid partition view");
        }
        if (memcmp(view.data, source + offset, view.len) != 0) {
            fail("partition bytes differ from fixture");
        }
        if (index + 1 < count && view.data[view.len - 1] != '\n') {
            fail("non-final partition splits a record");
        }
        offset += view.len;
    }
    if (offset != source_length) {
        fail("partition plan does not reconstruct the fixture");
    }

    if (mmap_engine_partition_records(handle, 0, '\n') != 0) {
        fail("N=0 unexpectedly succeeded");
    }
    const char *error = mmap_engine_last_error();
    if (error == NULL || strcmp(error, "requested_partitions must be > 0") != 0) {
        fail("N=0 error contract mismatch");
    }

    mmap_engine_free(handle);
    free(source);
    puts("PASS: GLIBC 2.17 runtime C consumer");
    return EXIT_SUCCESS;
}
