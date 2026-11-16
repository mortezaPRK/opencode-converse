//! Adds an `LC_RPATH` for the sherpa-onnx dylibs.
//!
//! `sherpa-rs-sys` with the `download-binaries` feature downloads
//! `sherpa-onnx-v1.12.9-osx-universal2-shared` and copies
//! `libsherpa-onnx-c-api.dylib`, `libsherpa-onnx-cxx-api.dylib` and
//! `libonnxruntime.1.17.1.dylib` next to the built binary (`target/<profile>/`,
//! `target/<profile>/deps/`, `target/<profile>/examples/`) but adds **no** `LC_RPATH`.
//! Without this the binary dies at startup with:
//!
//! ```text
//! dyld: Library not loaded: @rpath/libonnxruntime.1.17.1.dylib
//!       ... no LC_RPATH's found
//! ```
//!
//! `@executable_path` covers binaries and examples, `@loader_path` covers test
//! binaries under `target/<profile>/deps/`; in both cases the dylibs are already
//! sitting in the same directory.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
        // Test and example binaries live one level down (`target/<profile>/deps`,
        // `target/<profile>/examples`); the dylibs are in `target/<profile>` as well.
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/..");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/..");
    }
}
