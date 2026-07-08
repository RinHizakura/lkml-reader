.PHONY: build run-reader clean clean-full

LIST ?= lkml

build:
	cargo build --release

run-reader: build
	./target/release/lkml-reader -l $(LIST)

clean:
	cargo clean

clean-full: clean
	rm -rf "$${XDG_CACHE_HOME:-$$HOME/.cache}/lkml-tools"
