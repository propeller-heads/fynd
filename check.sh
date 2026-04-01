set -e

cargo +nightly fmt -- --check
cargo +nightly clippy --locked --all --all-features --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked --package fynd-core --package fynd-rpc-types --package fynd-rpc --package fynd-client
cargo nextest run --workspace --locked --all-targets --all-features --bin fynd