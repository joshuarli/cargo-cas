PYTHON ?= python3
CARGO ?= cargo
PREFIX ?= $(HOME)/.cargo-cas
BINDIR ?= $(PREFIX)/bin
CARGO_CAS_BUILD_FLAGS ?= --features all-static,cargo-cas-default

.PHONY: demo benchmark-ish path-demo install

demo: benchmark-ish

benchmark-ish:
	$(CARGO) build -p cargo --release
	$(PYTHON) scripts/benchmark-ish-cas.py

path-demo:
	$(CARGO) build -p cargo --release
	$(PYTHON) scripts/demo-path-cas.py

# Install beside, rather than over, rustup's cargo proxy. Put BINDIR first on
# PATH to make this release binary the cargo users invoke locally.
install:
	$(CARGO) build -p cargo --release $(CARGO_CAS_BUILD_FLAGS)
	install -d "$(BINDIR)"
	install -m 755 target/release/cargo "$(BINDIR)/cargo"
	@echo "Installed cargo-cas as $(BINDIR)/cargo"
	@echo "Add it before rustup's cargo on PATH:"
	@echo "  export PATH=\"$(BINDIR):\$$PATH\""
