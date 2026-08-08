# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a Vulnerability

This project uses **private vulnerability reporting** on GitHub.

To report a security issue:

1. Go to the [Security tab](https://github.com/trungminhdo4-glitch/mmap-chunker-core/security) of the repository.
2. Click **"Report a vulnerability"**.
3. Provide a clear description, reproduction steps, and impact assessment.

**Do not open a public issue** for vulnerabilities that could be exploited.

We aim to acknowledge reports within 5 business days and provide an initial
assessment. Response times depend on severity and maintainer availability.

## Scope

Security reports are welcome for:

- Memory safety issues in unsafe code (use-after-free, buffer overflows, null dereferences)
- C ABI contract violations (struct layout mismatches, calling convention errors)
- Panic escape from FFI boundaries
- Resource leaks (file descriptors, handles, mappings)
- Build or supply-chain integrity issues

## Out of scope

- Performance characteristics (throughput, latency) unless exploitable as DoS
- File mutation by external processes (documented as undefined behavior)
- Path truncation or encoding issues already covered by the UTF-8 contract
