dnl phpize/PIE build of the mongodb extension (mongo-php-async-driver). The extension is pure
dnl Rust: ./configure only checks for cargo, and `make` runs `cargo build` (Makefile.frag) and
dnl puts the result in modules/mongodb.so, where `make install` and PIE expect it.

PHP_ARG_ENABLE([mongodb],
  [whether to build the mongodb extension (mongo-php-async-driver)],
  [AS_HELP_STRING([--enable-mongodb], [Build the mongodb extension (needs Rust: cargo)])],
  [yes], [no])

if test "$PHP_MONGODB" != "no"; then
  AC_PATH_PROG([CARGO], [cargo], [no], [$PATH:$HOME/.cargo/bin])
  if test "$CARGO" = "no"; then
    AC_MSG_ERROR([cargo was not found: this extension is written in Rust, install it from https://rustup.rs (or set CARGO=/path/to/cargo)])
  fi
  PHP_SUBST([CARGO])
  PHP_SUBST([PHP_CONFIG])
  PHP_ADD_MAKEFILE_FRAGMENT([$abs_srcdir/Makefile.frag], [$abs_srcdir], [$abs_builddir])
fi
