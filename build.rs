use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=OPENSSL_DIR");
    println!("cargo:rerun-if-env-changed=OPENSSL_LIB_DIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if let Some(dir) = env_lib_dir() {
        emit(&dir);
        return;
    }
    if let Some(dir) = pkg_config_lib_dir() {
        emit(&dir);
        return;
    }
    for dir in [
        "/opt/local/libexec/openssl3/lib",
        "/opt/homebrew/opt/openssl@3/lib",
        "/usr/local/opt/openssl@3/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib64",
        "/usr/lib",
    ] {
        if lib_exists(dir) {
            emit(dir);
            return;
        }
    }
    println!("cargo:rustc-link-lib=ssl");
    println!("cargo:rustc-link-lib=crypto");
}

fn env_lib_dir() -> Option<String> {
    if let Ok(dir) = std::env::var("OPENSSL_LIB_DIR") {
        if lib_exists(&dir) {
            return Some(dir);
        }
    }
    if let Ok(dir) = std::env::var("OPENSSL_DIR") {
        let lib = Path::new(&dir).join("lib");
        if lib_exists(lib.to_str()?) {
            return Some(lib.to_string_lossy().into_owned());
        }
    }
    None
}

fn pkg_config_lib_dir() -> Option<String> {
    let output = Command::new("pkg-config")
        .args(["--libs", "openssl"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    for tok in text.split_whitespace() {
        if let Some(dir) = tok.strip_prefix("-L") {
            if lib_exists(dir) {
                return Some(dir.to_string());
            }
        }
    }
    None
}

fn lib_exists(dir: &str) -> bool {
    let base = Path::new(dir);
    [
        "libssl.dylib",
        "libssl.3.dylib",
        "libssl.so",
        "libssl.so.3",
        "libssl.a",
    ]
    .iter()
    .any(|name| base.join(name).exists())
}

fn emit(dir: &str) {
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=ssl");
    println!("cargo:rustc-link-lib=crypto");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{dir}");
    println!("cargo:rustc-link-arg-tests=-Wl,-rpath,{dir}");
}
