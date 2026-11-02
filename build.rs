use std::process::Command;

fn main() {
    #[cfg(feature = "pipewire")]
    {
        let library = pkg_config::Config::new()
            .cargo_metadata(false)
            .atleast_version("1.4")
            .probe("libpipewire-0.3")
            .expect("PipeWire development files are required for --features pipewire");
        cc::Build::new()
            .file("src/audio/events.c")
            .includes(library.include_paths)
            .warnings(true)
            .opt_level(2)
            .flag("-std=gnu11")
            .compile("pc2mqtt_audio_events");
        for path in library.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
        for name in library.libs {
            println!("cargo:rustc-link-lib={name}");
        }
        println!("cargo:rerun-if-changed=src/audio/events.c");
    }
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
