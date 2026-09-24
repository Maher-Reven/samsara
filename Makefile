CARGO ?= cargo
WASM_OUT := web/pkg

.PHONY: all test lint demo example example-paced demo-svg web web-test shim clean

all: test lint

test:
	$(CARGO) test --features testkit

lint:
	$(CARGO) clippy --all-targets --features testkit -- -D warnings
	$(CARGO) fmt --check

demo:
	$(CARGO) run --bin samsara -- demo

## The worked example, end to end, from nothing. Builds what it needs.
example: | .example-deps
	cd examples/document-cleanup && ./demo.sh

## The same walkthrough, pausing between beats, for presenting live.
example-paced: | .example-deps
	cd examples/document-cleanup && ./demo.sh --paced

.example-deps:
	$(CARGO) build --release
	cd shim/typescript && npm install --silent && npm run build --silent
	cd examples/document-cleanup && npm install --silent

## Regenerate the README's terminal image from a live run, so it cannot
## drift away from what the tool actually prints.
demo-svg: | .example-deps
	cd examples/document-cleanup && ./svg.sh

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
