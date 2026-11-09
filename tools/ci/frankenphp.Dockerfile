# The official driver's FrankenPHP restart test (tests/phpt/frankenphp/restart-test.sh, PHPC-2797)
# against this extension: its Dockerfile builds the C extension, this one builds ours the way PIE
# does (phpize, configure, make: cargo) into the same FrankenPHP image, whose PHP is thread-safe.
#   docker build --tag mongodb-frankenphp --file tools/ci/frankenphp.Dockerfile .
#   MONGODB_URI=mongodb://host.docker.internal:27017/?directConnection=true SKIP_BUILD=1 \
#     bash tests/phpt/frankenphp/restart-test.sh
FROM dunglas/frankenphp:1.13.4-php8.5

RUN apt-get update && \
    apt-get install --yes --no-install-recommends build-essential clang libclang-dev curl ca-certificates && \
    rm -rf /var/lib/apt/lists/*
RUN curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH

COPY . /usr/src/mongodb
WORKDIR /usr/src/mongodb
RUN phpize && \
    ./configure --with-php-config="$(command -v php-config)" && \
    make && \
    make install && \
    docker-php-ext-enable mongodb && \
    rm -rf target /root/.cargo/registry

COPY tests/phpt/frankenphp/Caddyfile /etc/frankenphp/Caddyfile
COPY tests/phpt/frankenphp/worker.php /app/public/worker.php
