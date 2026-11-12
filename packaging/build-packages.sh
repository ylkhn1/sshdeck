#!/bin/sh
# Собирает пакеты .deb (Debian 13) и .rpm (Fedora 42) в контейнерах Docker,
# чтобы они были слинкованы с библиотеками этих систем и получили их
# зависимости. Результат — в dist/.
#
#   packaging/build-packages.sh          # оба
#   packaging/build-packages.sh deb      # только .deb
#   packaging/build-packages.sh rpm      # только .rpm
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
what=${1:-all}

# Общая часть: Rust через rustup и копия исходников без target/ и dist/.
common='
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
. "$HOME/.cargo/env"
mkdir /build
cd /src && tar --exclude=./target --exclude=./dist --exclude=./.git -cf - . | tar -C /build -xf -
cd /build
'

run() {
    image=$1
    script=$2
    docker run --rm \
        -v "$PWD:/src:ro" -v "$PWD/dist:/out" \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        "$image" sh -euc "$script"
}

if [ "$what" = all ] || [ "$what" = deb ]; then
    echo "==> .deb (debian:trixie)"
    run debian:trixie "
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    curl ca-certificates build-essential pkg-config dpkg-dev \
    libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libsqlite3-dev libssl-dev >/dev/null
$common
cargo install cargo-deb --locked -q
cargo deb --locked
cp target/debian/*.deb /out/
chown \"\$HOST_UID:\$HOST_GID\" /out/*.deb
"
fi

if [ "$what" = all ] || [ "$what" = rpm ]; then
    echo "==> .rpm (fedora:42)"
    run fedora:42 "
dnf install -y -q gcc pkgconf-pkg-config rpm-build curl tar \
    gtk4-devel libadwaita-devel vte291-gtk4-devel sqlite-devel openssl-devel >/dev/null
$common
cargo install cargo-generate-rpm --locked -q
cargo build --release --locked
cargo generate-rpm
cp target/generate-rpm/*.rpm /out/
chown \"\$HOST_UID:\$HOST_GID\" /out/*.rpm
"
fi

ls -l dist/*.deb dist/*.rpm 2>/dev/null || true
