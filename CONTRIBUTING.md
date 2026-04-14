# Contributing to Babel3

Thank you for your interest in contributing to Babel3! This document explains the process.

## Developer Certificate of Origin (DCO)

All contributions must include a `Signed-off-by` line in the commit message, certifying you have the right to submit the code under Apache 2.0:

```
Signed-off-by: Your Name <your.email@example.com>
```

Use `git commit -s` to add this automatically. Our CI will block PRs without it.

By signing off, you certify (per [DCO 1.1](https://developercertificate.org/)):
1. You wrote the contribution (or have the right to submit it)
2. You're submitting it under the project's Apache-2.0 license
3. You have the right to make this submission

## How to Contribute

### Bug Reports

Open an issue with:
- Steps to reproduce
- Expected vs actual behavior
- Version (`b3 version`)
- Platform (OS, architecture)

### Pull Requests

1. Fork the repository on GitHub
2. Create a feature branch (`git checkout -b feature/my-change`)
3. Make your changes
4. Sign your commits (`git commit -s -m "Description"`)
5. Push to your fork (`git push origin feature/my-change`)
6. Open a PR against `main`

### What We're Looking For

- Bug fixes with test coverage
- Documentation improvements
- New GPU worker job types
- Browser UI enhancements
- Voice pipeline improvements
- MCP server extensions

### Code Style

- **Rust:** Follow `cargo fmt` and `cargo clippy`
- **JavaScript:** No framework — vanilla JS with the `HC` namespace
- **CSS:** CSS custom properties (variables) for theming
- **Commits:** Clear, descriptive messages. One logical change per commit.

## Running Tests

```bash
# Rust tests
cargo test -p b3-cli
cargo test -p b3-common

# GPU worker tests (requires Docker + GPU)
cd gpu-worker && docker build -t babel3-gpu-test . && docker run babel3-gpu-test pytest

# Browser — no build step needed (vanilla JS, no bundler)
# Open browser/js/ files directly or serve via the daemon
```

## Review Process

Every PR is reviewed for:
- **Security:** No secrets, credentials, or internal URLs in the code
- **License compliance:** All dependencies compatible with Apache-2.0
- **Functionality:** Does it work? Does it break existing functionality?
- **Style:** Follows the code conventions above

Maintainers may suggest improvements — these are non-blocking unless they affect security or license compliance.

## License

By contributing, your code is licensed under Apache-2.0. See [LICENSE](LICENSE).
