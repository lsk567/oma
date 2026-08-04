.PHONY: build install uninstall test-topology dev

BINARIES := omar omar-computer omar-slack
INSTALL_DIR := $(HOME)/.cargo/bin

build:
	cargo build --release

install: build
	install -d $(INSTALL_DIR)
	install $(addprefix target/release/,$(BINARIES)) $(INSTALL_DIR)/

uninstall:
	rm -f $(addprefix $(INSTALL_DIR)/,$(BINARIES))

test-topology:
	OMAR_TEST_CASE="$(CASE)" ./tests/topology/run_local.sh

# The daemon and Mission Control together, pointed at each other. Ctrl-C stops
# both. Needs Node; see web/README.md.
dev:
	./web/dev.sh
