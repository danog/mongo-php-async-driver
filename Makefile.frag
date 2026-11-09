# Included by the phpize Makefile (config.m4): the extension is built by cargo.
all: modules/mongodb.so
build-modules: modules/mongodb.so

.PHONY: mongodb-cargo-build
mongodb-cargo-build:
	cd "$(srcdir)" && PHP="$(PHP_EXECUTABLE)" PHP_CONFIG="$(PHP_CONFIG)" "$(CARGO)" build --release --locked

modules/mongodb.so: mongodb-cargo-build
	@mkdir -p modules
	@lib="$(srcdir)/target/release/libmongodb.so"; \
	[ -f "$$lib" ] || lib="$(srcdir)/target/release/libmongodb.dylib"; \
	cp "$$lib" $@
