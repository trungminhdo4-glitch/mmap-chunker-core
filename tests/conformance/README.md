# Linux C ABI conformance corpus

The four consumers in this directory all open the same `corpus.jsonl` through
the public C ABI. The fixture has four uneven UTF-8 records and deliberately
ends with a newline (the present-final-newline case). CI copies it to a
directory whose name contains a non-ASCII character before passing the path to
each consumer.

`expected.txt` is one canonical, human-readable result line. Each consumer
derives the values independently from the native library, verifies complete
byte-for-byte reconstruction and a consumer-side FNV-1a checksum, then writes
the same line. CI compares the four output files byte-for-byte.

The partition plan requests `N=4` and uses newline as the record delimiter; the
source-head library returns three non-empty partitions for this corpus. The
layout
assertions describe the Linux x86_64 ABI targeted by this job: `CChunkView` is
16 bytes with `data` at offset 0 and `len` at offset 8.
