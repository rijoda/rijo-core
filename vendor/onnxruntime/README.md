# Bundled runtime: onnxruntime.dll

The brain (fastembed) and the tool router use ONNX Runtime through the `ort`
crate in `load-dynamic` mode on Windows GNU. That mode needs a matching
`onnxruntime.dll` at runtime, and a **mismatched system DLL aborts the process**
(measured 2026-10-29: a 1.17.1 on `PATH` panicked the first embedding with
`BadVersion`). So the DLL is vendored in the **open** repository instead of hoped
for. It is third-party MIT software, not part of the closed desktop app.

| | |
|---|---|
| File | `onnxruntime.dll` |
| Version | 1.28.0 (build `20260724.14.da9b5e3`) |
| Size | 15,809,848 bytes |
| SHA-256 | `18370C375F07357FA5874344A9D9AC17E6B6FE1EB18B1DD209D79483B4470257` |
| Licence | MIT — Microsoft ONNX Runtime (`microsoft/onnxruntime`, win-x64 build) |
| Why in the repo | reproducible builds; no network step and no dependence on whatever runtime an unrelated installer left on the machine |

## Where it is found at runtime

`rijo_core::router::session::ensure_ort_dll_loaded` sets `ORT_DYLIB_PATH` from
the first existing candidate:

1. beside the executable (`target/debug`, `target/release`, installed app root),
2. one directory up (a test binary in `target/debug/deps`),
3. `resources/` next to the executable (what the Tauri bundle installs),
4. `<rijo_home>/bin` (`~/.rijo/bin`, the CLI installer's location).

The root `.cargo/config.toml` also points `ORT_DYLIB_PATH` at this file, so
`cargo test` and `cargo run` work from a fresh checkout — for the open workspace and
for the closed app repository as well.

The desktop bundle maps this file to `onnxruntime.dll` beside the executable, so the
first runtime candidate finds it without a second copy in the app repository.

## Replacing it

Take the win-x64 release of Microsoft ONNX Runtime that matches the `ort`
version in `Cargo.lock`, replace the file, update the table (version, size,
hash), and run `cargo test -p rijo-core brain::embed`.

Keep the DLL architecture at x64; the app is x64 on Windows.
