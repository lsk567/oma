.PHONY: build install uninstall test-topology dev web-bundle

BINARIES := omar omar-computer omar-slack
INSTALL_DIR := $(HOME)/.cargo/bin

# Mission Control, built and compressed for the runtime to embed. Needs Node.
web-bundle:
	cd web && npm ci && npm run build:spa

# Built with the UI in it, so an installed `omar serve --ui` has something to
# serve. A plain `cargo build --release` still works and still needs no Node;
# it just cannot serve the UI.
build: web-bundle
	cargo build --release --features ui

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
