<p align="center"><img src="assets/logos/logo.png" alt="FileCraft logo: an orange folder with a magnifying glass" width="160"></p>

<h1 align="center">FileCraft</h1>

Instant file search for Windows: a desktop app in the spirit of
[Everything](https://www.voidtools.com/), written in Rust. It indexes every file and folder on your
fixed drives and filters millions of entries as you type.

## Features

- **Searches all drives at once**, with results sortable by name, path, size and date.
- **NTFS MFT indexing**: with **⚡ Fast NTFS indexing** on, a small elevated helper reads each
  drive's master file table directly, which indexes a drive in seconds; the window itself never
  needs administrator rights. Otherwise it uses a parallel folder scan.
- **Instant startup**: indexes are cached on disk and refreshed in the background.
- **Everything-style queries**: `ext:pdf`, `size:>100mb`, `dm:today`, `folder:src`, wildcards,
  `!not`, `a|b`, path terms and `regex:`.
- **Preview and edit**: text with an inline editor (Ctrl+S to save; encoding and line endings
  kept), images, hex dumps and folder listings.
- **Shell actions**: open, show in Explorer, Properties, copy path and external editor.
- **Quick search bar**: press **Alt+Space** anywhere for a small launcher-style search box that
  ranks programs and close matches first. FileCraft stays in the notification area when its window
  is closed.

Measured on a machine with 7 drives and 13 million files, without admin rights:

| | |
| --- | --- |
| Folder scan of a 4.2 M-entry SSD | 18.7 s |
| Load that drive from cache | 0.35 s |
| Typical search | 7–90 ms |
| All 13 M entries, unfiltered | 35 ms |
| Memory use | ~1.6 GB |

## Build & run

```powershell
cargo build --release
.\target\release\FileCraft.exe
```

Click **⚡ Fast NTFS indexing** in the status bar for MFT indexing (one UAC prompt for the helper).

```powershell
cargo test                                                        # unit tests
$env:FILECRAFT_BENCH_ROOT = "D:\"; cargo test --release bench_real_drive -- --ignored --nocapture
```

## Documentation

- [User guide](docs/user-guide.md): window, search syntax, editing, shortcuts, settings
- [Architecture](docs/architecture.md): index layout, MFT reader, search engine
- [Changelog](CHANGELOG.md)

## Contributing

Changes go through pull requests: create a branch, push it and open a PR against `main`. Every
PR needs one approving review before it can be merged.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) builds, lints (`clippy -D warnings`)
and tests on Windows in both the debug and release profiles, and checks dependencies with
`cargo deny check` (policy in [`deny.toml`](deny.toml)) and `cargo audit`. It also runs weekly, so
new advisories show up between pull requests. Third-party actions are pinned to commit SHAs and
kept current by Dependabot. Run the same checks locally before pushing:

```powershell
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo deny check
```

## License

FileCraft is free software: you can redistribute it and/or modify it under the terms of the
[GNU General Public License](LICENSE) as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version.

FileCraft is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
[LICENSE](LICENSE) file for details.
