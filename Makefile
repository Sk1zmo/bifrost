# Bifrost — common tasks.
#
#   make            build the release binary
#   make run        build and run a node
#   make test       run every test
#   make check      fmt + clippy + test, what CI runs
#   make android    cross-compile the core for Android
#   make ios        cross-compile the core for iOS (macOS only)
#   make install    build and install locally

CARGO   ?= cargo
PREFIX  ?= /usr/local/bin
BINARY  := target/release/bifrost

.DEFAULT_GOAL := build
.PHONY: build run serve test check fmt lint clean install uninstall \
        android ios linux windows docker help

build: ## Build the release binary
	$(CARGO) build --release -p bifrost-cli
	@echo "Built $(BINARY)"

run: build ## Build and run a node with both dashboards
	$(BINARY)

serve: build ## Build and run headless
	$(BINARY) serve

test: ## Run every test
	$(CARGO) test --workspace

fmt: ## Format
	$(CARGO) fmt --all

lint: ## Lint, warnings are errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: ## What CI runs
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) test --workspace

clean:
	$(CARGO) clean

install: build ## Install to $(PREFIX)
	install -d $(PREFIX)
	install -m 755 $(BINARY) $(PREFIX)/bifrost
	@echo "Installed $(PREFIX)/bifrost"

uninstall:
	rm -f $(PREFIX)/bifrost

# ── Cross-compilation ───────────────────────────────────────────────────────
# The mobile targets build only the core as a shared/static library; the app
# shells are built by Flutter and Xcode.

android: ## Build libbifrost_core.so for Android (needs ANDROID_NDK_HOME)
	./scripts/build-android.sh

ios: ## Build libbifrost_core.a for iOS (macOS only)
	./scripts/build-ios.sh

linux: ## Cross-compile the binary for Linux x86-64
	rustup target add x86_64-unknown-linux-gnu
	$(CARGO) build --release -p bifrost-cli --target x86_64-unknown-linux-gnu

windows: ## Cross-compile the binary for Windows x86-64
	rustup target add x86_64-pc-windows-gnu
	$(CARGO) build --release -p bifrost-cli --target x86_64-pc-windows-gnu

docker: ## Build the container image
	docker build -t bifrost:latest .

help: ## List these targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[1m%-10s\033[0m %s\n", $$1, $$2}'
