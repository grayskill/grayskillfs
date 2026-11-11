.DEFAULT_GOAL := all

CARGO ?= cargo
CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR

ifneq ($(filter clean distclean,$(MAKECMDGOALS)),)
ifneq ($(filter-out clean distclean help,$(MAKECMDGOALS)),)
$(error Run clean/distclean separately from build, test, or lint targets)
endif
endif

.PHONY: all test test-mounted do-lint clean distclean help
.PHONY: check-cargo check-build check-test check-mounted check-lint FORCE

all: $(CARGO_TARGET_DIR)/release/grayskillfs

# Cargo tracks source, configuration, toolchain and flag changes, including removals.
# Always let it check freshness rather than maintaining a second Rust dependency graph.
$(CARGO_TARGET_DIR)/release/grayskillfs: Cargo.toml Cargo.lock FORCE | check-build
	+$(CARGO) build --release --locked

test: check-test
	+$(CARGO) test --all-targets --locked

test-mounted: check-mounted
	+$(CARGO) test --locked --test mounted -- --ignored --nocapture

do-lint: check-lint
	$(CARGO) fmt --all --check
	+$(CARGO) clippy --all-targets --locked -- -D warnings

clean: check-cargo
	$(CARGO) clean

distclean: clean

check-cargo:
	@command -v "$(CARGO)" >/dev/null 2>&1 || { \
		printf '%s\n' 'error: Cargo is required; install Rust 1.85+ with Cargo, or enter nix-shell.' >&2; exit 1; }
	@$(CARGO) --version >/dev/null

check-build: check-cargo
	@test "$$(uname -s)" = Linux || { \
		printf '%s\n' 'error: grayskillfs requires Linux.' >&2; exit 1; }
	@command -v "$${RUSTC:-rustc}" >/dev/null 2>&1 || { \
		printf '%s\n' 'error: rustc is required; install Rust 1.85+.' >&2; exit 1; }
	@command -v "$(CC)" >/dev/null 2>&1 || { \
		printf '%s\n' 'error: a C compiler/linker is required; install cc or set CC.' >&2; exit 1; }

check-test: check-build
	@command -v git >/dev/null 2>&1 || { \
		printf '%s\n' 'error: Git is required for the local repository tests.' >&2; exit 1; }

check-mounted: check-test
	@command -v fusermount3 >/dev/null 2>&1 || { \
		printf '%s\n' 'error: install FUSE 3 (fusermount3) to run mounted tests.' >&2; exit 1; }
	@command -v make >/dev/null 2>&1 || { \
		printf '%s\n' 'error: make must be on PATH for the mounted build test.' >&2; exit 1; }
	@test -c /dev/fuse && test -r /dev/fuse && test -w /dev/fuse || { \
		printf '%s\n' 'error: mounted tests require a readable/writable /dev/fuse device and permission to mount FUSE filesystems.' >&2; exit 1; }

check-lint: check-build
	@$(CARGO) fmt --version >/dev/null 2>&1 || { \
		printf '%s\n' 'error: rustfmt is required; install it with rustup component add rustfmt.' >&2; exit 1; }
	@$(CARGO) clippy --version >/dev/null 2>&1 || { \
		printf '%s\n' 'error: Clippy is required; install it with rustup component add clippy.' >&2; exit 1; }

FORCE:

help:
	@printf '%s\n' \
		'make              Build the release binary (default: target/release/grayskillfs).' \
		'make test         Run automated tests without requiring a FUSE mount.' \
		'make test-mounted Run live FUSE tests (Linux 5.8+, /proc, /dev/fuse, FUSE 3, Git).' \
		'make do-lint      Check formatting and run strict Clippy linting.' \
		'make clean        Remove Cargo build outputs.' \
		'make distclean    Remove all generated build files (same as clean); keep Cargo.lock.' \
		'make help         Show this help.' \
		'' \
		'Build/test/lint targets support make -j; run cleanup separately.' \
		'Override CARGO or CARGO_TARGET_DIR to select Cargo or its output directory.'
