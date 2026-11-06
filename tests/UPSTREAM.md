# Carried-over test suites

These directories are copies of upstream test suites, kept as close to upstream as possible so
that they can be re-synced. Local changes, if any, are listed below per suite.

| Directory | Upstream | Tag / commit | Licence |
|---|---|---|---|
| `phpt/` | https://github.com/mongodb/mongo-php-driver `tests/` (without the `drivers-evergreen-tools` submodule) | `2.5.6` / `4ff9840052151eb8d5477149a3700a01bca10b60` | Apache-2.0 |
| `phplib/tests/`, `phplib/phpunit.xml.dist`, `phplib/composer.json` | https://github.com/mongodb/mongo-php-library `tests/` (without submodules) | `2.5.1` / `f5fdaaf13c0f8f9b3e59ba52a47d430533858843` | Apache-2.0 |
| `spec/` (git submodule, not copied) | https://github.com/mongodb/specifications | `eb2ee1e529cf1c2fbf8bd54c32dc32c644306d0d` (the commit PHPLIB 2.5.1 pins) | CC BY-NC-SA 3.0 US |

The specifications repository is licensed CC BY-NC-SA 3.0 US, so its files are not copied into
this Apache-2.0 repository: `spec/` is a git submodule pinned to that commit (as PHPLIB does),
and `phplib/tests/specifications` is a symlink to it. Run `git submodule update --init` first.

## Re-syncing

```sh
rsync -a --delete --exclude drivers-evergreen-tools <mongo-php-driver>/tests/ tests/phpt/
rsync -a --delete --exclude drivers-evergreen-tools --exclude specifications <mongo-php-library>/tests/ tests/phplib/tests/
git -C tests/spec fetch origin <commit> && git -C tests/spec checkout <commit>
```

## Local changes

None yet.
