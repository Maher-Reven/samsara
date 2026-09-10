CARGO ?= cargo
WASM_OUT := web/pkg

.PHONY: all test lint demo web web-test shim clean

all: test lint

test:
	$(CARGO) test --features testkit

lint:
	$(CARGO) clippy --all-targets --features testkit -- -D warnings
	$(CARGO) fmt --check

demo:
	$(CARGO) run --bin samsara -- demo

## Build the WebAssembly engine and bindings for the browser timeline.
web:
	$(CARGO) build -p samsara-wasm --target wasm32-unknown-unknown --release
	wasm-bindgen --target web --no-typescript \
		--out-dir $(WASM_OUT) \
		target/wasm32-unknown-unknown/release/samsara_wasm.wasm
	@echo "serve with: python3 -m http.server -d web 8000"

## Assert the page and the engine still agree on their JSON contract.
## Uses the nodejs bindings: same Rust, different glue.
web-test:
	$(CARGO) build -p samsara-wasm --target wasm32-unknown-unknown --release
	wasm-bindgen --target nodejs --no-typescript \
		--out-dir web/pkg-node \
		target/wasm32-unknown-unknown/release/samsara_wasm.wasm
	node --test web/test/

shim:
	cd shim/typescript && npm install && npm run build

clean:
	$(CARGO) clean
	rm -rf $(WASM_OUT) web/pkg-node traces shim/typescript/dist shim/typescript/node_modules
