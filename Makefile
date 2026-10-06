.PHONY: all check fmt clean package
all:
	cargo build --locked --offline --release
check:
	cargo test --locked --offline --all-targets
	cargo build --locked --offline --example oracle --bin macpro-fand
	python3 tests/parity.py target/debug/examples/oracle
	python3 tests/cli.py target/debug/macpro-fand
fmt:
	cargo fmt --check
	cargo clippy --locked --offline --all-targets -- -D warnings
clean:
	cargo clean
package:
	dpkg-buildpackage -us -uc -b
