use std::process::Command;

fn main() {
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(
        std::env::var_os("PROTOC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| protoc_bin_vendored::protoc_bin_path().unwrap()),
    );
    tonic_prost_build::configure()
        .compile_with_config(config, &["proto/comin.proto"], &["proto"])
        .expect("compile Comin protocol");
    println!("cargo:rerun-if-changed=proto/comin.proto");
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok();

    let sha = output
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            println!("cargo:warning=could not determine git SHA; using 'unknown'");
            "unknown".to_string()
        });

    println!("cargo:rustc-env=GIT_SHA={sha}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/");
}
