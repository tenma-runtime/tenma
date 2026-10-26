#!/bin/sh
# Builds tenma-<version>-1.el10.<arch>.rpm inside packaging/el10.Dockerfile's image, into the directory $2:
#   docker run --rm -v "$PWD:/out" tenma-el10 packaging/rpm.sh 0.2.10 /out
# rpmbuild finds the libraries the binary links and requires them by soname, so dnf installs them.
set -eu
version=$1 out=$2
top=$(mktemp -d)
mkdir -p "$top/SOURCES"
cp target/release/tenma NOTICE LICENSE-MIT LICENSE-APACHE "$top/SOURCES/"
cat > "$top/tenma.spec" <<SPEC
Name: tenma
Version: $version
Release: 1%{?dist}
Summary: PHP as a long-running process: the script runs once, then one event loop serves every request
License: MIT OR Apache-2.0
URL: https://tenma.dev
Requires: ca-certificates
# The binary as built: no stripping, no debuginfo package
%global debug_package %{nil}
%global __os_install_post %{nil}

%description
One process runs the PHP script once and then serves requests continuously,
every request in its own fiber, with async Postgres, Redis, files and WebSockets.

%install
install -Dm755 %{_sourcedir}/tenma %{buildroot}/usr/bin/tenma
install -Dm644 %{_sourcedir}/NOTICE %{buildroot}/usr/share/doc/tenma/NOTICE
install -Dm644 %{_sourcedir}/LICENSE-MIT %{buildroot}/usr/share/licenses/tenma/LICENSE-MIT
install -Dm644 %{_sourcedir}/LICENSE-APACHE %{buildroot}/usr/share/licenses/tenma/LICENSE-APACHE

%files
/usr/bin/tenma
/usr/share/doc/tenma/NOTICE
/usr/share/licenses/tenma/LICENSE-MIT
/usr/share/licenses/tenma/LICENSE-APACHE
SPEC
rpmbuild -bb --define "_topdir $top" "$top/tenma.spec"
cp "$top"/RPMS/*/tenma-*.rpm "$out/"
