BIN := target/release/sph

.PHONY: build test check clean

build:
	cargo build --release

test:
	cargo test

check:
	cargo fmt --check
	cargo clippy --all-targets

clean:
	rm -rf target $(BIN)
