#!/bin/sh
# Собирает файлы выпуска в контейнерах Docker, чтобы программа была
# слинкована с библиотеками этих систем и пакеты получили их зависимости.
# Результат — в dist/:
#
#   sshdeck_X.Y.Z-1_amd64.deb            Debian 13 (debian:trixie)
#   sshdeck-X.Y.Z-1.x86_64.rpm           Fedora 42 (fedora:42)
#   sshdeck-X.Y.Z-x86_64-linux.tar.gz    архив для других систем (debian:trixie)
#
#   packaging/build-packages.sh          # всё
#   packaging/build-packages.sh deb      # только .deb
#   packaging/build-packages.sh rpm      # только .rpm
#   packaging/build-packages.sh tarball  # только архив
#
# Каждая сборка знает, как её установили (SSHDECK_INSTALL_KIND), — от
# этого зависит, как программа обновляется. Поэтому программа в архиве
# собирается отдельно, а не берётся из .deb.
#
# Программа встраивает ключ packaging/minisign.pub, которым проверяет
# обновления; с временным ключом разработчика сборка отказывается
# работать (см. packaging/check-release-key.sh). Для пробной сборки, которая
# никуда не публикуется: SSHDECK_ALLOW_DEV_KEY=1.
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
what=${1:-all}
case $what in
all | deb | rpm | tarball) ;;
*) echo "Использование: $0 [deb|rpm|tarball]" >&2; exit 1 ;;
esac

if ! packaging/check-release-key.sh >/dev/null; then
    if [ "${SSHDECK_ALLOW_DEV_KEY:-}" = 1 ]; then
        echo "!!! Пробная сборка с ключом разработчика — НЕ ПУБЛИКОВАТЬ !!!" >&2
    else
        exit 1
    fi
fi
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)

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

# Debian: .deb и программа для архива — в одном контейнере.
debian_deps='
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    curl ca-certificates build-essential pkg-config dpkg-dev \
    libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libsqlite3-dev libssl-dev >/dev/null
'
deb_part='
cargo install cargo-deb --locked -q
SSHDECK_INSTALL_KIND=deb cargo deb --locked
cp target/debian/*.deb /out/
chown "$HOST_UID:$HOST_GID" /out/*.deb
'
# Архив: sshdeck-X.Y.Z/ с программой, ярлыком, значком, README и лицензией.
tarball_part="
SSHDECK_INSTALL_KIND=tarball cargo build --release --locked
dir=/tmp/sshdeck-$version
mkdir -p \"\$dir\"
install -m755 target/release/sshdeck \"\$dir/sshdeck\"
install -m644 data/dev.sshdeck.App.desktop data/dev.sshdeck.App.svg README.md LICENSE \"\$dir/\"
tar -C /tmp --owner=0 --group=0 --numeric-owner -czf /out/sshdeck-$version-x86_64-linux.tar.gz sshdeck-$version
chown \"\$HOST_UID:\$HOST_GID\" /out/sshdeck-$version-x86_64-linux.tar.gz
"

debian_script=
if [ "$what" = all ] || [ "$what" = deb ]; then
    debian_script=$deb_part
fi
if [ "$what" = all ] || [ "$what" = tarball ]; then
    debian_script="$debian_script$tarball_part"
fi
if [ -n "$debian_script" ]; then
    echo "==> $what (debian:trixie)"
    run debian:trixie "$debian_deps$common$debian_script"
fi

if [ "$what" = all ] || [ "$what" = rpm ]; then
    echo "==> .rpm (fedora:42)"
    run fedora:42 "
dnf install -y -q gcc pkgconf-pkg-config rpm-build curl tar \
    gtk4-devel libadwaita-devel vte291-gtk4-devel sqlite-devel openssl-devel >/dev/null
$common
cargo install cargo-generate-rpm --locked -q
SSHDECK_INSTALL_KIND=rpm cargo build --release --locked
cargo generate-rpm
cp target/generate-rpm/*.rpm /out/
chown \"\$HOST_UID:\$HOST_GID\" /out/*.rpm
"
fi

ls -l dist/*.deb dist/*.rpm dist/*.tar.gz 2>/dev/null || true
