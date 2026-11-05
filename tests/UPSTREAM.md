# Carried-over test suites

These directories are copies of upstream test suites, kept as close to upstream as possible so
that they can be re-synced. Local changes, if any, are listed below per suite.

| Directory | Upstream | Tag / commit | Licence |
|---|---|---|---|
| `phpt/` | https://github.com/mongodb/mongo-php-driver `tests/` (without the `drivers-evergreen-tools` submodule) | `2.5.6` / `4ff9840052151eb8d5477149a3700a01bca10b60` | Apache-2.0 |
| `phplib/tests/`, `phplib/phpunit.xml.dist`, `phplib/composer.json` | https://github.com/mongodb/mongo-php-library `tests/` (without submodules) | `2.5.0` / `d70875dbb897a1cc7de15b6d21ceed43706f1937` | Apache-2.0 |
| `spec/` (git submodule, not copied) | https://github.com/mongodb/specifications | `f5ba7f7aaa0417e35671a3ed7a8cd26be3a8e5ba` (the commit PHPLIB 2.5.0 pins) | CC BY-NC-SA 3.0 US |

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
