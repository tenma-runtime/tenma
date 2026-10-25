#!/bin/sh
# Builds PHP as a static library with the embed SAPI and ZTS into php-build/out.
# tenma links libphp.a in, the system PHP stays untouched.
#
# Libraries for extensions are linked dynamically. On macOS they come from the SDK (libxml2, zlib,
# sqlite, curl) and from Homebrew:
#   brew install pkgconf openssl@3 libpq icu4c libsodium libzip gmp oniguruma libpng jpeg-turbo webp freetype
# On Linux from the distribution's -dev packages (Debian's are in the Dockerfile).
# ponytail: the binary only runs where these libraries are installed; the Docker image brings them
set -eu
VERSION=8.5.12
SHA256=08bc2eba2a588654ade1357174aa646b761556c862dda0678345ee29ed481209
cd "$(dirname "$0")"

if [ ! -f "php-$VERSION.tar.xz" ]; then
  curl -sSfLO "https://www.php.net/distributions/php-$VERSION.tar.xz"
fi
echo "$SHA256  php-$VERSION.tar.xz" | shasum -a 256 -c -
rm -rf "php-$VERSION" && tar xf "php-$VERSION.tar.xz"

cd "php-$VERSION"
if [ "$(uname -s)" = Darwin ]; then
  SDK="$(xcrun --show-sdk-path)"
  BREW="$(brew --prefix)"
  export LIBXML_CFLAGS="-I$SDK/usr/include/libxml2" LIBXML_LIBS="-lxml2"
  export ZLIB_CFLAGS="" ZLIB_LIBS="-lz"
  export SQLITE_CFLAGS="" SQLITE_LIBS="-lsqlite3"
  export CURL_CFLAGS="" CURL_LIBS="-lcurl"
  # Homebrew does not put some packages in the default path (keg-only)
  for package in openssl@3 libpq icu4c libsodium libzip gmp oniguruma libpng jpeg-turbo webp freetype; do
    PKG_CONFIG_PATH="$BREW/opt/$package/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
  done
  export PKG_CONFIG_PATH
  PGSQL="=$BREW/opt/libpq" GMP="=$BREW/opt/gmp" CPUS="$(sysctl -n hw.ncpu)"
else
  # Linux: everything is found through pkg-config and the default paths
  PGSQL="" GMP="" CPUS="$(nproc)"
fi

./configure \
  --prefix="$(pwd)/../out" \
  --enable-embed=static \
  --enable-zts \
  --disable-all \
  --disable-cli --disable-cgi --disable-phpdbg \
  `# JIT: PHP does not build it with ZTS on Apple Silicon ("JIT not supported on Apple Silicon with ZTS"),` \
  `# on Linux it does; enable with -d opcache.jit=tracing` \
  --enable-opcache-jit \
  `# Basics` \
  --enable-ctype --enable-tokenizer --enable-filter --enable-fileinfo --enable-session \
  --enable-bcmath --enable-calendar --enable-exif --enable-posix --enable-sockets \
  --with-zlib --enable-mbstring --without-iconv \
  `# XML` \
  --with-libxml --enable-simplexml --enable-dom --enable-xml --enable-xmlreader --enable-xmlwriter \
  `# Databases` \
  --enable-pdo --with-pdo-mysql=mysqlnd --with-mysqli=mysqlnd --enable-mysqlnd \
  --with-pdo-sqlite --with-sqlite3 \
  --with-pdo-pgsql$PGSQL --with-pgsql$PGSQL \
  `# Network and cryptography` \
  --with-openssl --with-curl --with-sodium \
  `# Other` \
  --enable-intl --with-gmp$GMP --with-zip --enable-phar \
  --enable-gd --with-jpeg --with-webp --with-freetype
make -j"$CPUS"
make install
