.PHONY: all build run clean clean-full

all: build $(GIT_HOOKS)

$(GIT_HOOKS):
	@scripts/install-git-hooks
	@echo

build:
	cargo build

run:
	cargo run

clean:
	cargo clean

clean-full: clean
	rm -rf "$${XDG_CACHE_HOME:-$$HOME/.cache}/lkml-reader"
