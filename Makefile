# Local build and install of plannotator-tui from this checkout.
#
# `make uninstall` restores the latest published plannotator-tui release from crates.io.
# `make uninstall-main` restores the latest commit of upstream main instead.
# Set ORIGINAL_VERSION=x.y.z to pin a release for `make uninstall`.

UPSTREAM_URL ?= https://github.com/plannotator/plannotator-tui
ORIGINAL_VERSION ?=
VERSION_FLAG := $(if $(ORIGINAL_VERSION),--version $(ORIGINAL_VERSION),)

.PHONY: help build install uninstall uninstall-main

help:
	@echo "make build           build the release binary at target/release/plannotator-tui"
	@echo "make install         install this checkout's plannotator-tui to ~/.cargo/bin (replaces the current one)"
	@echo "make uninstall       remove it and reinstall the latest plannotator-tui release from crates.io"
	@echo "make uninstall-main  remove it and reinstall the latest commit of upstream main"

build:
	cargo build --release -p plannotator-tui

install:
	cargo install --path crates/plannotator-tui --locked --force

uninstall:
	-cargo uninstall plannotator-tui
	cargo install plannotator-tui $(VERSION_FLAG) --locked --force

uninstall-main:
	-cargo uninstall plannotator-tui
	cargo install --git $(UPSTREAM_URL) --locked --force plannotator-tui
