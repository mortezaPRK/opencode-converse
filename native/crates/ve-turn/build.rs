//! Links Apple's compiler-rt builtins library when we statically link the prebuilt
//! ONNX Runtime archive on macOS.
//!
//! Identical to `crates/ve-vad/build.rs`;
//! each crate carries its own copy so that either one can be built on its own with
//! `cargo build -p ve-turn`. The linker tolerates the directives being emitted twice when
//! both crates end up in the same binary.
//!
//! Background: the prebuilt `libonnxruntime.a` has ONNX Runtime's CoreML execution provider
//! compiled in, and those Objective-C++ objects reference `___isPlatformVersionAtLeast`,
//! which clang normally supplies from `libclang_rt.osx.a`. Rust drives the linker with
//! `-nodefaultlibs`, so the archive is never pulled in and the link fails. `-lclang_rt.osx`
//! alone is not enough — clang's resource directory is not on the default library search
//! path when `cc` acts as a plain linker driver — so we ask clang where it is and add
//! `<resource-dir>/lib/darwin` explicitly.

use std::process::Command;

// Parse only a bounded local compiler invocation; never run a shell. Preserve
// quoted paths and wrapper/compiler arguments when appending the probe flag.
fn compiler_command(cc: &str) -> Option<Command> {
    if cc.len() > 8192 || cc.contains('\0') {
        return None;
    }
    if std::path::Path::new(cc).is_file() {
        return Some(Command::new(cc)); // An unquoted executable path may contain spaces.
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = cc.chars();
    while let Some(ch) = chars.next() {
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            } else if delimiter == '"' && ch == '\\' {
                let next = chars.next()?;
                if matches!(next, '"' | '\\' | '$' | '`') {
                    word.push(next);
                } else if next != '\n' {
                    word.push('\\');
                    word.push(next);
                }
            } else {
                word.push(ch);
            }
        } else if ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
                if words.len() > 64 {
                    return None;
                }
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            started = true;
        } else if ch == '\\' {
            let next = chars.next()?;
            if next != '\n' {
                word.push(next);
                started = true;
            }
        } else {
            word.push(ch);
            started = true;
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    if words.is_empty() || words.len() > 64 || words[0].is_empty() {
        return None;
    }
    let mut command = Command::new(&words[0]);
    command.args(&words[1..]);
    Some(command)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        return;
    }

    // Prefer an explicit CC, else plain `clang`.
    let cc = std::env::var("CC").unwrap_or_else(|_| "clang".to_string());
    let out = compiler_command(&cc)
        .and_then(|mut command| command.arg("-print-resource-dir").output().ok());

    let resource_dir = match out {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            println!(
                "cargo:warning=could not probe selected CC resource directory; static ORT link may fail"
            );
            return;
        }
    };

    let darwin_lib = std::path::Path::new(&resource_dir).join("lib/darwin");
    if !darwin_lib.join("libclang_rt.osx.a").exists() {
        println!(
            "cargo:warning=libclang_rt.osx.a not found in {}; static ORT link may fail",
            darwin_lib.display()
        );
        return;
    }

    println!("cargo:rustc-link-search=native={}", darwin_lib.display());
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_and_quoted_arguments_are_preserved_without_a_shell() {
        let command = compiler_command("ccache clang --target=aarch64-apple-darwin").unwrap();
        assert_eq!(command.get_program(), "ccache");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["clang", "--target=aarch64-apple-darwin"]
        );
        let command =
            compiler_command("'wrapper path' \"compiler path\" '--extra=space value'").unwrap();
        assert_eq!(command.get_program(), "wrapper path");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["compiler path", "--extra=space value"]
        );
        let command = compiler_command(r#""C:\Program Files\clang.exe" --flag="a\"b""#).unwrap();
        assert_eq!(command.get_program(), r"C:\Program Files\clang.exe");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--flag=a\"b"]);
    }

    #[test]
    fn malformed_and_unbounded_invocations_are_rejected() {
        for cc in ["", "''", "'unterminated", r"clang \", "clang\0bad"] {
            assert!(compiler_command(cc).is_none());
        }
        assert!(compiler_command(&"x".repeat(8193)).is_none());
        assert!(compiler_command(&"clang ".repeat(65)).is_none());
    }
}
