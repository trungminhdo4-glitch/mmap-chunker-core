package main

/*
#cgo CFLAGS: -I${SRCDIR}/../../..
#include "mmap_chunker.h"
#include <stdlib.h>
*/
import "C"

import (
	"flag"
	"fmt"
	"os"
	"strings"
	"unsafe"
)

const (
	fnvOffset uint64 = 14695981039346656037
	fnvPrime  uint64 = 1099511628211
)

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "FAIL: "+format+"\n", args...)
	os.Exit(1)
}

func lastError() string {
	message := C.mmap_engine_last_error()
	if message == nil {
		return ""
	}
	return C.GoString(message)
}

func fnv1a(chunks [][]byte) uint64 {
	digest := fnvOffset
	for _, chunk := range chunks {
		for _, byteValue := range chunk {
			digest ^= uint64(byteValue)
			digest *= fnvPrime
		}
	}
	return digest
}

func capture(handle *C.CEngineHandle, source []byte) ([][]byte, uint64) {
	count := C.mmap_engine_partition_records(handle, C.size_t(4), C.uint8_t('\n'))
	if count == 0 {
		fail("unexpected partition count")
	}
	chunks := make([][]byte, 0, int(count))
	offset := 0
	for index := C.size_t(0); index < count; index++ {
		var view C.CChunkView
		if C.mmap_engine_get_chunk(handle, index, &view) != C.int32_t(0) {
			fail("could not retrieve partition")
		}
		length := int(view.len)
		if view.data == nil || length <= 0 || offset+length > len(source) {
			fail("invalid partition view")
		}
		chunk := C.GoBytes(unsafe.Pointer(view.data), C.int(length))
		if string(chunk) != string(source[offset:offset+length]) {
			fail("partition bytes differ from fixture")
		}
		if index+1 < count && chunk[len(chunk)-1] != '\n' {
			fail("non-final partition splits a record")
		}
		chunks = append(chunks, chunk)
		offset += length
	}
	if offset != len(source) {
		fail("partition plan does not reconstruct the fixture")
	}
	return chunks, fnv1a(chunks)
}

func main() {
	library := flag.String("library", "", "authoritative native library path")
	fixture := flag.String("fixture", "", "fixture path")
	expected := flag.String("expected", "", "canonical result path")
	output := flag.String("output", "", "result output path")
	flag.Parse()
	if *library == "" || *fixture == "" || *expected == "" || *output == "" {
		fail("usage requires --library, --fixture, --expected, and --output")
	}
	if _, err := os.Stat(*library); err != nil {
		fail("authoritative library path is not readable: %v", err)
	}

	if C.size_t(unsafe.Sizeof(C.CChunkView{})) != C.size_t(16) {
		fail("CChunkView size mismatch")
	}
	var layout C.CChunkView
	if unsafe.Offsetof(layout.data) != 0 || unsafe.Offsetof(layout.len) != 8 {
		fail("CChunkView offset mismatch")
	}
	if C.mmap_engine_abi_version() != C.uint32_t(0x00010003) ||
		C.mmap_engine_capabilities() != C.uint32_t(63) {
		fail("ABI discovery mismatch")
	}

	source, err := os.ReadFile(*fixture)
	if err != nil {
		fail("could not read fixture: %v", err)
	}
	path := C.CString(*fixture)
	defer C.free(unsafe.Pointer(path))
	handle := C.mmap_engine_open(path)
	if handle == nil {
		fail("mmap_engine_open failed for UTF-8 path")
	}
	first, digest := capture(handle, source)
	second, repeatDigest := capture(handle, source)
	if len(first) != len(second) || digest != repeatDigest {
		fail("partition plan is not deterministic")
	}
	for index := range first {
		if string(first[index]) != string(second[index]) {
			fail("partition bytes are not deterministic")
		}
	}
	if C.mmap_engine_partition_records(handle, C.size_t(0), C.uint8_t('\n')) != 0 {
		fail("N=0 unexpectedly succeeded")
	}
	n0Error := lastError()
	if n0Error != "requested_partitions must be > 0" {
		fail("N=0 error contract mismatch: %q", n0Error)
	}
	C.mmap_engine_free(handle)

	recordCount := strings.Count(string(source), "\n")
	if len(source) > 0 && source[len(source)-1] != '\n' {
		recordCount++
	}
	lengths := make([]string, len(first))
	for index, chunk := range first {
		lengths[index] = fmt.Sprintf("%d", len(chunk))
	}
	result := fmt.Sprintf(
		"abi_version=65539;capabilities=63;partition_count=%d;partition_lengths=%s;"+
			"total_length=%d;record_count=%d;fnv1a64=%016x;deterministic=1;"+
			"n0_error=%s;chunk_view_size=%d;chunk_view_data_offset=0;chunk_view_len_offset=8",
		len(first), strings.Join(lengths, ","), len(source), recordCount, digest,
		n0Error, unsafe.Sizeof(C.CChunkView{}))
	expectedBytes, err := os.ReadFile(*expected)
	if err != nil {
		fail("could not read expected result: %v", err)
	}
	if result != strings.TrimSpace(string(expectedBytes)) {
		fail("canonical result mismatch\nexpected: %s\nactual:   %s", strings.TrimSpace(string(expectedBytes)), result)
	}
	if err := os.WriteFile(*output, []byte(result+"\n"), 0644); err != nil {
		fail("could not write result: %v", err)
	}
	fmt.Println("PASS: Go/cgo conformance consumer")
}
