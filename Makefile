.PHONY: build install uninstall test-topology

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
