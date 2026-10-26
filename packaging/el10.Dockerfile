# tenma built against Enterprise Linux 10 (CentOS Stream 10, RHEL 10, AlmaLinux 10, Rocky 10) for its .rpm:
#   docker build -f packaging/el10.Dockerfile -t tenma-el10 .
#   docker run --rm -v "$PWD:/out" tenma-el10 packaging/rpm.sh 0.2.10 /out
FROM quay.io/centos/centos:stream10
# CRB and EPEL carry the -devel packages the base repositories leave out (oniguruma, libsodium, libzip)
RUN dnf install -y dnf-plugins-core epel-release && dnf config-manager --set-enabled crb \
    && dnf install -y gcc gcc-c++ make perl curl xz pkgconf-pkg-config rpm-build \
    libxml2-devel zlib-ng-compat-devel sqlite-devel libcurl-devel openssl-devel libpq-devel \
    libicu-devel libsodium-devel libzip-devel gmp-devel oniguruma-devel libpng-devel libjpeg-turbo-devel \
    libwebp-devel freetype-devel \
    && dnf clean all
# The same toolchain as the Debian build's image
ENV PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.0 --no-modify-path
WORKDIR /src
COPY php-build/build.sh php-build/
# Debian's gcc builds position-independent code by default, this one doesn't: Rust links a PIE
RUN CFLAGS="-g -O2 -fPIE" CXXFLAGS="-g -O2 -fPIE" php-build/build.sh
COPY . .
RUN cargo build --release --locked
