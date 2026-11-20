# Credential disclosure corpus

`credential-disclosure.json` is copied without modification from libmongoc 2.5.9
(`src/libmongoc/tests/json/credential_disclosure/`, mongo-c-driver commit
`78e3c959caab2d4e5c86b4a18ec7951ba216d946`, Apache-2.0), which copied it from the DRIVERS-3623
connection-string tests. `../uri_disclosure.rs` runs it as libmongoc's
`test-mongoc-connection-uri.c` does, with the driver tests that come with it there.
