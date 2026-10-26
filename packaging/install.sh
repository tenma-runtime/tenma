#!/bin/sh
# Installs the latest tenma: curl -fsSL https://raw.githubusercontent.com/tenma-runtime/tenma/main/packaging/install.sh | sh
# macOS through Homebrew (the binary links Homebrew's libraries), Debian 13 and Ubuntu 24.04 through their .deb,
# Enterprise Linux 10 (CentOS Stream, RHEL, AlmaLinux, Rocky) through its .rpm with EPEL,
# any other Linux as the binary in ~/.local/bin (TENMA_DIR), with a list of the libraries it still misses.
# TENMA_VERSION=0.1.2 installs that version on Linux.
set -eu
repo=tenma-runtime/tenma
fail() { echo "tenma: $*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin)
    [ "$(uname -m)" = arm64 ] || fail "only Apple Silicon Macs are supported"
    command -v brew >/dev/null || fail "needs Homebrew (https://brew.sh), then: brew install tenma-runtime/tap/tenma"
    exec brew install tenma-runtime/tap/tenma
    ;;
  Linux) ;;
  *) fail "$(uname -s) is not supported" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch=x64 debarch=amd64 ;;
  aarch64 | arm64) arch=arm64 debarch=arm64 ;;
  *) fail "$(uname -m) is not supported" ;;
esac
version=${TENMA_VERSION:-}
# The redirect of releases/latest names the tag; the API would count against its hourly limit per IP,
# which shared CI runners use up
[ -n "$version" ] || version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest" | sed -n 's|.*/tag/v\(.*\)$|\1|p')
[ -n "$version" ] || fail "cannot find the latest release"
download="https://github.com/$repo/releases/download/v$version"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT

if command -v apt-get >/dev/null; then
  # Ubuntu 24.04 has its own build; Debian 13 and everything newer take Debian's
  . /etc/os-release
  case "$ID$VERSION_ID" in
    ubuntu24.04) deb="tenma_${version}_ubuntu24.04_$debarch.deb" ;;
    *) deb="tenma_${version}_$debarch.deb" ;;
  esac
  curl -fsSL -o "$temp/$deb" "$download/$deb"
  sudo=""; [ "$(id -u)" = 0 ] || sudo=sudo
  # apt resolves the libraries; on a release without them (older Debian or Ubuntu) it says which are missing
  $sudo apt-get install -y "$temp/$deb"
elif command -v dnf >/dev/null && grep -qx 'PLATFORM_ID="platform:el10"' /etc/os-release; then
  # Enterprise Linux 10 has its own build; some of the libraries it links come from EPEL,
  # which RHEL itself does not carry as a package
  rpm="tenma-$version-1.el10.$(uname -m).rpm"
  curl -fsSL -o "$temp/$rpm" "$download/$rpm"
  sudo=""; [ "$(id -u)" = 0 ] || sudo=sudo
  if ! rpm -q epel-release >/dev/null; then
    $sudo dnf install -y epel-release \
      || $sudo dnf install -y https://dl.fedoraproject.org/pub/epel/epel-release-latest-10.noarch.rpm
  fi
  $sudo dnf install -y "$temp/$rpm"
else
  dir=${TENMA_DIR:-$HOME/.local/bin}
  curl -fsSL "$download/tenma-linux-$arch.tar.gz" | tar xz -C "$temp"
  mkdir -p "$dir"
  # rm first: overwriting a running binary in place fails or breaks it
  rm -f "$dir/tenma" && cp "$temp/tenma" "$dir/tenma"
  missing=$(ldd "$dir/tenma" 2>/dev/null | sed -n 's/^[[:space:]]*\([^ ]*\) => not found/\1/p')
  case ":$PATH:" in *":$dir:"*) ;; *) echo "tenma: add $dir to your PATH" >&2 ;; esac
  [ -z "$missing" ] || fail "installed in $dir, but these libraries are missing (or use the Docker image ghcr.io/tenma-runtime/tenma):" $missing
  "$dir/tenma" --version
  exit
fi
tenma --version
