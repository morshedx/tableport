CONFIG := release
APP_BUNDLE := .build/TablePort.app

# Build, signing and release come from mac-kit, checked out next to this repo.
MACKIT ?= ../mac-kit/bin/mackit

# Machine-specific settings, not in git. For example:
#   MACKIT_SIGN = Apple Development: you@example.com (TEAMID1234)
-include local.mk
export MACKIT_SIGN
CARGO ?= $(HOME)/.cargo/bin/cargo

.PHONY: all core core-test build app run install clean setup-dev-cert icon

all: app

# The Rust core (crates/core) as a static library, plus its Swift bindings,
# which UniFFI generates from the library: Sources/TablePortCore/tableport_core.swift
# and the C header in Sources/tableport_coreFFI. The workspace builds into target/.
core:
	$(CARGO) build --release -p tableport-core
	$(CARGO) run --release --quiet -p tableport-core --bin uniffi-bindgen -- generate \
		--library target/release/libtableport_core.dylib --language swift --out-dir target/swift
	cp target/swift/tableport_core.swift Sources/TablePortCore/tableport_core.swift
	cp target/swift/tableport_coreFFI.h Sources/tableport_coreFFI/tableport_coreFFI.h
	cp target/swift/tableport_coreFFI.modulemap Sources/tableport_coreFFI/module.modulemap

# Needs a local PostgreSQL server that trusts the current user.
core-test:
	TABLEPORT_TEST_PG=1 $(CARGO) test --workspace

build: core
	swift build -c $(CONFIG)

# AppIcon.icns from the SVGs in Icon/ (detailed drawing from 64 px, simple
# one below). Run after changing either SVG; the .icns is checked in.
icon:
	swift scripts/make-icon.swift "$(CURDIR)"

# Build + sign the .app bundle from mackit.toml.
app: core
	@$(MACKIT) build

run: core
	@$(MACKIT) run

install: core
	@$(MACKIT) install

# One-time: create a self-signed codesigning cert so the signature stays
# stable across rebuilds. Builds fall back to ad-hoc without it.
setup-dev-cert:
	@$(MACKIT) cert

clean:
	swift package clean
	$(CARGO) clean
	rm -rf "$(APP_BUNDLE)"
