fn main() {
    // sherpa-rs-sys copies its shared libraries alongside the executable.
    // Dependency build scripts cannot set the final executable's rpath.
    if std::env::var_os("CARGO_FEATURE_NATIVE_AUDIO").is_some() {
        match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
            Ok("macos") => {
                println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
                println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/..");
            }
            Ok("linux") => println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN"),
            _ => {}
        }
    }
}
