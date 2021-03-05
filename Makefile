all: bin/glcim

bin/glcim: $(shell find -name \*.rs) Cargo.toml
	@./build.sh
