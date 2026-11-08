# Release checklist

## One-time (repo preparation)

- [x] `.gitignore` covers `target/`, the deployed binary `/rijo`, test artifacts
      and `.env`
- [x] History no longer contains 50 MB binaries or model caches (pack: 475 KiB;
      a backup bundle of the old history lives outside the repo)
- [x] `LICENSE-MIT` + `LICENSE-APACHE` (dual `MIT OR Apache-2.0`, matching
      `Cargo.toml`), `TRADEMARKS.md`, `CONTRIBUTING.md`, `DCO`, `SECURITY.md`,
      `CHANGELOG.md`, `docs/INSTALL.md`
- [x] `install.sh` (source and binary install, `--check`, `--dry-run`,
      `--uninstall`)
- [x] `[profile.release]` moved to the root manifest — it used to sit in a
      member crate, where cargo silently ignores it (no LTO at all)
- [x] CI (`.github/workflows/ci.yml`) and release workflow (`release.yml`)
- [x] Cargo metadata via `[workspace.package]` (license, repository, rust-version)

## Per release

1. Update `CHANGELOG.md` (version, date, content, known limitations).
2. Bump the version in `Cargo.toml` (`[workspace.package] version`).
3. Tests green:
   ```bash
   cargo test --workspace
   ./install.sh --check
   ```
4. Release build and smoke test:
   ```bash
   cargo build --release -p rijo
   ./target/release/rijo --version
   RIJO_SKIP_TRUST=1 RIJO_SKIP_ONBOARD=1 ./target/release/rijo --tools | head -3
   ```
5. Commit, tag, push:
   ```bash
   git tag -a v0.1.0 -m "Rijo Code v0.1.0"
   git push origin master --follow-tags
   ```
6. The `Release` workflow builds and attaches `rijo-<arch>-linux.tar.gz` plus
   `.sha256` to the GitHub release. Verify with `gh release view v0.1.0`.
   (Until the first tag this workflow is **unverified**.)
7. Announcement: check and extend the release notes GitHub generated
   (`--generate-notes` builds them from the commits).

## What a developer needs to try it

```bash
git clone https://github.com/rijoda/rijo-core.git && cd rijo-core && ./install.sh
```

Without a Rust toolchain, the release asset works too:

```bash
./install.sh --binary v0.1.0
```

## Honest limits worth stating in the release notes

- Linux-first, macOS untested.
- The Windows release asset is a **bare `rijo.exe`** (plus the ONNX Runtime
  requirement): `install.ps1` has no `--binary` mode yet, so a Windows user
  without a Rust toolchain unzips it and places `onnxruntime.dll` next to it by
  hand. The Linux `install.sh --binary <tag>` path is the only automated
  asset install.
- The guard is a heuristic, not a sandbox (`SECURITY.md`).
- The benefit of the learning loop is not proven yet (an A/B with 5+5 runs on a
  pitfall task is pending, see `docs/history/goal-100-de.md` section Q).
- `debug.rs` always writes the tool trace to `~/.rijo/data/`, even with a
  different `data_dir`.
