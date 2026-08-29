# Contributing

Thank you for your interest in and contributions to Osmium!

## Project Structure Notes

- **Single Rust implementation**: implemented in Rust (edition 2024), producing `osmium64.exe`.
- **Inline strings**: user-facing messages and log strings are written directly at their usage site in the code — edit the source to change the wording.
- **Version**: `Project/Cargo.toml` is the single source of truth for the version; `.release.ps1` syncs it to `installer.iss`.

## Development Flow

1. Fork this repository and create a feature branch
2. Modify the code
3. Verify locally: run `.\.release.ps1` (includes Rust unit tests)
4. Commit and open a Pull Request

## Code Style

- Comments no longer than two lines; fold overly long single-line comments into two lines
- Check after every edit: remove redundant / dead code, merge mergeable code, clean up unused `use` imports
- Do not break security essentials: service name validation (prevents path traversal), tightened deployment directory ACL, etc.

## Commit Messages

Use clear Chinese or English to describe the change, e.g. "fix updater stop hang".
