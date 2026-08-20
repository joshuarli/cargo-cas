PYTHON ?= python3
CARGO ?= cargo

.PHONY: demo benchmark-ish path-demo

demo: benchmark-ish

benchmark-ish:
	$(CARGO) build -p cargo --release
	$(PYTHON) scripts/benchmark-ish-cas.py

path-demo:
	$(CARGO) build -p cargo --release
	$(PYTHON) scripts/demo-path-cas.py
