binary := "target/x86_64-unknown-linux-musl/release/pc2mqtt"
deploy := "/opt/pc2mqtt/pc2mqtt"

check:
    cargo fmt --check
    cargo clippy
    cargo audit
    cargo machete

build:
    cargo clean
    cargo build --release

deploy: build
    sudo install -m 755 {{binary}} {{deploy}}.new
    sudo mv -f {{deploy}}.new {{deploy}}

restart: deploy
    sudo systemctl restart pc2mqtt

fmt:
    cargo fmt

bloat:
    cargo bloat --release
