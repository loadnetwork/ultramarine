cargo clippy --workspace --lib --examples --tests --benches --locked --all-features
cargo +nightly fmt
cargo check --all

