-include Makefile.options
log?=INFO
###############################################################################
run:
	RUST_LOG=$(log) cargo run --bin mr --
.PHONY: run
###############################################################################
run/build: build/local
	RUST_LOG=$(log) target/release/mr 
.PHONY: run/build
###############################################################################
build/local: 
	cargo build --release
.PHONY: build/local
###############################################################################
build/lib: 
	cargo build --lib --release
.PHONY: build/lib
###############################################################################
test/unit:
	RUST_LOG=DEBUG cargo test --no-fail-fast
.PHONY: test/unit		
test/lint:
	@cargo clippy -V
	cargo clippy --all-targets --all-features -- -D warnings
.PHONY: test/lint	
test/coverage:
	cargo tarpaulin --ignore-tests
.PHONY: test/coverage
.PHONY: test/unit	
test/format:
	cargo fmt -- --check
.PHONY: test/format
audit:
	cargo audit
.PHONY: audit
install/checks:
	cargo install cargo-audit
	cargo install cargo-tarpaulin
.PHONY: install/checks
###############################################################################
clean:
	cargo clean
.PHONY: clean
