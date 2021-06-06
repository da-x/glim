all: bin/glim

bin/glim: $(shell find -name \*.rs) Cargo.toml
	@./build.sh
