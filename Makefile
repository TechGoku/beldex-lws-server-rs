# Builds the C++ kernel (and beldex core under it) once, then the Rust workspace
# against that prebuilt kernel.

KERNEL_BUILD ?= build/kernel
BUILD_TYPE   ?= Release
VERSION      := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
export LWS_KERNEL_LIB_DIR := $(CURDIR)/$(KERNEL_BUILD)

.PHONY: all kernel build release test bench fixture dist install fmt lint clean

all: release

$(KERNEL_BUILD)/build.ninja:
	git submodule update --init --recursive
	cmake -S kernel -B $(KERNEL_BUILD) -G Ninja -DCMAKE_BUILD_TYPE=$(BUILD_TYPE)

kernel: $(KERNEL_BUILD)/build.ninja
	cmake --build $(KERNEL_BUILD)

build: kernel
	cargo build --workspace

release: kernel
	cargo build --release -p beldex-lws

test: kernel
	$(KERNEL_BUILD)/lws_kernel_selftest
	cargo test --workspace

bench: kernel
	cargo test --release -p beldex-lws --test bench -- --ignored --nocapture

# Regenerates the chain the end-to-end test replays.
fixture: kernel
	$(KERNEL_BUILD)/lws_make_chain > crates/beldex-lws/tests/fixtures/chain.json

fmt:
	cargo fmt --all

lint: kernel
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

# bin/ and lib/ side by side; the binary finds the kernel through $ORIGIN/../lib.
dist: release
	rm -rf dist/beldex-lws-$(VERSION)
	mkdir -p dist/beldex-lws-$(VERSION)/bin dist/beldex-lws-$(VERSION)/lib
	cp target/release/beldex-lws dist/beldex-lws-$(VERSION)/bin/
	cp $(KERNEL_BUILD)/libbeldex_lws_kernel.so dist/beldex-lws-$(VERSION)/lib/
	cp -r README.md LICENSE docs deploy dist/beldex-lws-$(VERSION)/
	cp deploy/install.sh dist/beldex-lws-$(VERSION)/install.sh
	tar -C dist -czf dist/beldex-lws-$(VERSION)-linux-$(shell uname -m).tar.gz beldex-lws-$(VERSION)

# As root: installs the systemd service, configures it and starts it.
# Pass setup options through ARGS, e.g. make install ARGS="--yes --network test".
install:
	deploy/install.sh $(ARGS)

clean:
	cargo clean
	rm -rf build dist
