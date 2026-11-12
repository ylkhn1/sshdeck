# Converting UI strings to tr!() — brief for this pass

Project: sshdeck, a GTK4/libadwaita SSH client in Rust (edition 2024). Code comments are Russian
(leave them alone); user-visible strings are currently hard-coded Russian and must become English
source strings wrapped in the i18n macros, with Russian and Spanish translations in gettext catalogs.

Read `src/i18n.rs` (module doc at the top) first. Macros are global (`#[macro_use]`), no imports needed:
- `tr!("Text")` -> `&'static str`; `tr!("Hello, {}", name)` -> `String`. `{}` = next arg, `{0}`/`{1}` = by position.
- `trn!("{} row", "{} rows", n)` -> `String` (n fills the placeholder); `trn!("…", "…", n, a, b)` uses a, b instead.
- `trc!("context", "Text")` when one English text needs different translations in different places.
- `n_!("Text")` marks a string in a `const` table; translate at display time with `crate::i18n::tr(value)`.
Placeholders are plain Display: no `{x:.1}` specs inside templates — pre-format (`format!("{v:.1}")`) and pass it.

## What to do in your files (shipped code only = everything before `#[cfg(test)]`)
1. Every Russian string a person can see (labels, titles, buttons, tooltips, toasts, dialog text, error messages
   that reach the UI, placeholders, status texts) -> natural, concise English inside tr!/trn!/trc!/n_!.
   Write good English UI copy, not word-for-word translation.
2. Add each message to YOUR catalog parts (create them): `po/parts/<part>.ru.po` and `po/parts/<part>.es.po`.
   The Russian msgstr must be the ORIGINAL Russian text (same wording, punctuation, « », …), with format
   placeholders converted to `{}`/`{N}`. Spanish: translate from the meaning.
3. Not for people -> leave as is: ids, settings keys, protocol/JSON keys, CSS, shell snippets, SQL,
   markers like `SSHDECK_CWD_*`, log-only debug text that never reaches the UI.
4. `format!("Не удалось … {path}: {err}")` -> `tr!("Could not … {}: {}", path, err)`. Join string pieces built by
   concatenation into whole sentences so translators see full text.
5. Russian plural helpers (e.g. `plural(n, "строка", "строки", "строк")` in dbtab.rs) -> `trn!`. Russian gets 3 forms
   (one/few/many: 1 строка, 2 строки, 5 строк), Spanish 2.
6. If a Russian string is returned by one function and matched/compared somewhere else, grep for it (`grep -rn`)
   and keep both sides consistent — but only edit files assigned to you; report anything outside them.
7. Tests inside your files that assert Russian output: update to the English text (tests run without a catalog,
   so they see English and decimal separator '.'; use `crate::i18n::decimal_separator()` in number formatting
   that used a hard-coded ',').
8. `tr!` with no args is `&'static str`, with args `String`: adjust call sites (`&tr!(…)`, `.as_str()`, etc.).

## Catalog part format
```
msgid "Connect to “{}”"
msgstr "Подключение к «{}»"

msgid "{} row"
msgid_plural "{} rows"
msgstr[0] "{} строка"
msgstr[1] "{} строки"
msgstr[2] "{} строк"
```
Spanish part: same msgids, `msgstr[0]`/`msgstr[1]` for plurals. Escape `"` as `\"`, newline as `\n`, `\` as `\\`.
For trc!: put `msgctxt "context"` on the line before msgid. No duplicate msgids within your part.

## Style
- English: GNOME HIG. Header Capitalization for window/dialog titles, buttons, menu items, column headers;
  sentence case for everything else (descriptions, tooltips, toasts, form row titles, switches). Use “ ” quotes, … (U+2026).
- Spanish: neutral international Spanish, "tú", infinitive for buttons/menu items ("Guardar", "Conectar"),
  sentence case everywhere, « » quotes, … (U+2026), correct accents.
- Keep the same ellipsis/quote conventions as the Russian original where they carry meaning.

## Glossary (EN / ES)
connection / conexión · saved connections / conexiones guardadas · folder / carpeta · server / servidor · host / host ·
tab / pestaña · terminal / terminal · file panel / panel de archivos · upload / subir · download / descargar ·
rename / cambiar nombre · delete / eliminar · refresh / actualizar · hidden files / archivos ocultos ·
transfer / transferencia · reconnect / reconectar · passwordless login (key) / inicio de sesión sin contraseña (clave) ·
key / clave · passphrase / frase de contraseña · password / contraseña · password storage (keyring) / almacén de contraseñas ·
database / base de datos · database type (СУБД) / tipo de base de datos · database extensions / extensiones de bases de datos ·
extension / extensión · driver / controlador · query / consulta · run / ejecutar · result / resultado · row / fila ·
column / columna · table / tabla · view / vista · schema / esquema · export / exportar · import / importar ·
theme / tema · status bar / barra de estado · uptime / tiempo activo · load average / carga media ·
saved commands / comandos guardados · follow `cd` / seguir `cd` · Downloads folder / carpeta Descargas ·
online / en línea · offline / sin conexión · SSH tunnel / túnel SSH · primary key / clave primaria

## Rules of engagement
- Edit ONLY the files assigned to you plus your two `po/parts/<part>.*.po` files. Do not edit po/ru.po, po/es.po.
- Other agents are editing other files at the same time. `cargo build` / `cargo test` may show errors in files that
  are not yours — ignore those and re-run; your files must compile cleanly.
- Do NOT run `cargo fmt` or `rustfmt` (the repo is not rustfmt-formatted; it would reformat whole files). Keep the
  existing style by hand. Do not commit or touch git.
- Check you missed nothing: `cargo test --bin sshdeck no_untranslated 2>&1 | grep -E '^(<yourfile>)\.rs:'`
  (the test lists remaining Cyrillic literals by file name).
