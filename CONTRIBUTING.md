# Contributing

Report bugs and propose changes in
[the varies-kani repository](https://github.com/huanli-00/varies-kani).
Include the fork commit, platform, command, generated harness sources, sequence
metadata, and relevant output when reporting a verification problem.

Follow the source build instructions in [README.md](README.md). Before submitting
a pull request, run `cargo fmt`, focused unit tests, and the synthesized
smoke fixture. Describe changes to proof obligations, pruning, metadata, or
timeouts explicitly. Keep unrelated formatting changes separate.

See [AGENTS.md](AGENTS.md) for code boundaries and invariants,
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for community conduct, and
[the security policy](.github/SECURITY.md) for private vulnerability reports.

Contributions are licensed under MIT OR Apache-2.0. Preserve existing copyright
and license notices.
