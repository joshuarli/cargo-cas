PYTHON ?= python3
CARGO ?= cargo

.PHONY: demo

demo:
	$(CARGO) build -p cargo
	$(PYTHON) scripts/demo-path-cas.py
