# autoport - build & install
#
# Usage:
#   make            # release build
#   make install    # build, then copy the binary into BINDIR
#   make uninstall  # remove it again
#   make test       # run unit tests
#   make run HOST=172.18.128.11    # build + run against a host
#   make bench HOST=172.18.128.11  # throughput benchmark of many forwards
#
# Override the install location if you like:
#   make install BINDIR=/usr/local/bin

BIN     := autoport
BINDIR  ?= $(HOME)/.local/bin
TARGET  := target/release/$(BIN)
HOST    ?= 172.18.128.11

.PHONY: all build release install uninstall test run bench clean

all: release

build release: $(TARGET)

$(TARGET): $(wildcard src/*.rs) Cargo.toml Cargo.lock
	cargo build --release

install: release
	@mkdir -p "$(BINDIR)"
	install -m 0755 "$(TARGET)" "$(BINDIR)/$(BIN)"
	@echo "installed $(BIN) -> $(BINDIR)/$(BIN)"
	@command -v "$(BIN)" >/dev/null 2>&1 || echo "note: $(BINDIR) is not on your PATH"

uninstall:
	rm -f "$(BINDIR)/$(BIN)"
	@echo "removed $(BINDIR)/$(BIN)"

test:
	cargo test

run:
	cargo run --release -- $(HOST)

# Throughput of many concurrent forwards. Pass MODE=both to also measure
# one-connection-per-port, plus MAX=, SIZE_MB=, ROUNDS=, SWEEP= as needed.
bench:
	./bench/throughput.sh $(HOST)

clean:
	cargo clean
