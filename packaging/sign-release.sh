#!/bin/sh
# Подписывает файлы выпуска в dist/ ключом minisign и проверяет подписи
# открытым ключом из репозитория (тем, что встроен в программу).
#
#   packaging/sign-release.sh X.Y.Z
#
# Секретный ключ — ~/.minisign/sshdeck.key (или MINISIGN_KEY=<файл>);
# пароль от него спросит minisign. Рядом с каждым файлом появится
# <файл>.minisig — его нужно приложить к выпуску вместе с файлом.
#
# С временным ключом разработчика скрипт не работает — см.
# packaging/check-release-key.sh.
set -eu
cd "$(dirname "$0")/.."

version=${1:-}
if ! printf '%s\n' "$version" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+'; then
    echo "Использование: packaging/sign-release.sh X.Y.Z" >&2
    exit 1
fi

key_id=$(packaging/check-release-key.sh)
secret=${MINISIGN_KEY:-$HOME/.minisign/sshdeck.key}
command -v minisign >/dev/null || {
    echo "Нужна программа minisign (sudo pacman -S minisign)" >&2
    exit 1
}
[ -f "$secret" ] || { echo "Нет секретного ключа $secret" >&2; exit 1; }

# Те же имена, которые ищет программа (src/update/mod.rs, asset_name).
files="sshdeck-$version-x86_64-linux.tar.gz sshdeck_$version-1_amd64.deb sshdeck-$version-1.x86_64.rpm"
cd dist
for file in $files; do
    [ -f "$file" ] || { echo "Нет dist/$file — сначала packaging/build-packages.sh" >&2; exit 1; }
done

echo "==> Подпись ключом $key_id"
# Подписываем из dist/: в подписанный комментарий попадает имя файла
# без каталога, программа сверяет его с тем, что скачивала.
# shellcheck disable=SC2086
minisign -S -s "$secret" -m $files

echo "==> Проверка ключом packaging/minisign.pub"
for file in $files; do
    minisign -V -p ../packaging/minisign.pub -m "$file" -q
    if ! grep -q "file:$file" "$file.minisig"; then
        echo "В подписи $file.minisig нет имени файла (file:$file)" >&2
        exit 1
    fi
    echo "  $file: ok"
done
ls -l $files $(for file in $files; do printf '%s.minisig ' "$file"; done)
