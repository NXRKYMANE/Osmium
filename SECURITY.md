# Security Policy

## Supported Versions

The following versions receive security updates and vulnerability reports:

- The latest stable release

## Reporting a Vulnerability

Please **do not** disclose security vulnerabilities in public channels (Issues / Discussions / PRs). Report privately via:

- GitHub Security Advisory: repository page → **Security → Report a vulnerability**
- Maintainer email (NXRKYMANE)

Please include:

- Affected version
- Vulnerability description and impact
- Reproduction steps (if possible)
- Suggested fix (optional)

## Handling Process

- Acknowledge within 72 hours of receiving the report
- Fix and release a patch version as soon as possible after confirmation
- Vulnerability details are not disclosed until the fix is released

## Security Design

- Service name validation: rejects empty names, `.` / `..`, path separators and control characters (prevents path traversal)
- Tightened deployment directory ACL: only SYSTEM / Administrators are writable (prevents config tampering leading to arbitrary code execution)
- Refuses to start over plain HTTP without SHA-256 (prevents man-in-the-middle tampering)
