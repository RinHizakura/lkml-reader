.PHONY: build run-reader install uninstall clean clean-full

LIST ?= lkml
PREFIX ?= $(HOME)/.local

build:
	cargo build --release

install: build
	install -Dm755 target/release/lkml-reader $(DESTDIR)$(PREFIX)/bin/lkml-reader

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/lkml-reader

run-reader: build
	./target/release/lkml-reader -l $(LIST)

clean:
	cargo clean

clean-full: clean
	rm -rf "$${XDG_CACHE_HOME:-$$HOME/.cache}/lkml-tools"
