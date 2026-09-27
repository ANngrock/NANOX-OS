# Локальная среда M0

Основной checkout: `/home/holod/src/NANOX-OS`, WSL-дистрибутив `Ubuntu`.
Из Windows: `\\wsl.localhost\Ubuntu\home\holod\src\NANOX-OS`.
Ветка `codex/m0`, исходная история начинается с существующего HEAD `c5c3d7f`;
origin сохранён: `https://github.com/ANngrock/NANOX-OS.git`.

Исходный каталог в OneDrive остаётся сохранённым входным снимком. Не вести в
нём параллельную разработку. Перед переносом проверены `/home`, `/root`,
`/opt`, `/srv`, `/workspace`: существующий NANOX checkout не найден. История
клонирована из исходного репозитория; незакоммиченные файлы перенесены отдельно,
удаление `ARCHITECTURE.md` сохранено. SHA-256 перенесённых файлов записаны в
`.local/checkout-migration.json`. Дописанные перед перерывом файлы сверены и
перенесены 2026-09-23. Физические диски не изменялись.

Подтверждены Ubuntu 26.04, WSL2, Nix 2.35.2. WSL при запуске сообщает о сбое
NAT и использует собственный fallback VirtioProxy; запуск занимает несколько
минут. Настройки WSL не менялись.

## Вход в среду

```sh
cd /home/holod/src/NANOX-OS
nix develop
cargo xtask doctor
cargo xtask build
cargo xtask run
cargo xtask test --replay
cargo xtask reproduce-build
```

Для новых файлов среды выполнено `git add --intent-to-add flake.nix
flake.lock rust-toolchain.toml tools/qemu/replay-exit-code.patch`: Git-flake видит их текущее содержимое без commit
и без staged snapshot. Эти файлы нужны для оценки devShell. Cargo
читает остальные исходники непосредственно из текущего checkout. При первом
входе до этой операции допустим `nix develop path:.`; после появления больших
`out/` и `target/` этот вариант копирует лишние файлы в Nix store, поэтому
обычный Git-flake предпочтителен. Все сборки выполняются из Linux checkout;
target flags разделены между host, UEFI и bare-metal. Rust 1.90.0 и обе
target libraries поставляются одним закреплённым Nix toolchain.

QEMU 9.2.4 собирается с локальным патчем сохранения exit code в replay,
см. ADR-0002. Первый вход требует компиляции x86_64 QEMU; последующие
используют Nix store. Root-загрузка новой среды выполнялась через минимальный
`path:.local/nix-env`, содержащий точные копии четырёх входных файлов, поскольку
libgit2 запрещает root читать Git checkout другого владельца. Это только
staging файлов devShell; основной source/build checkout остаётся прежним.

## Локальный TLS

2026-09-23 Avast Web/Mail Shield выдавал свой сертификат для cache.nixos.org.
Windows доверяет его корню, Ubuntu — нет. Публичный корневой сертификат из
Windows Root store экспортирован в игнорируемый `.local/windows-trusted-avast.pem`
и добавлен к системному CA bundle в `.local/build-ca-bundle.pem`. Проверка TLS
с этим файлом проходит. Глобальные настройки доверия и антивирус не менялись.

Nix daemon считает `ssl-cert-file` restricted setting, поэтому первоначальная
загрузка зависимости в store требует доверенного root-вызова с этой опцией.
Публичный bundle предварительно добавлен через `nix store add-file`, чтобы
сборочные пользователи `nixbld` могли читать его, не меняя права домашней папки:

```powershell
wsl -d Ubuntu -u root -- bash -lc 'cd /home/holod/src/NANOX-OS && nix --option ssl-cert-file /nix/store/9bzwq86qgz44pmx9x3w45h5ijqawxvwb-build-ca-bundle.pem develop path:. --command true'
```

Компиляция и QEMU затем идут от пользователя `holod`. При загрузке Cargo
зависимостей на этом компьютере используется `CARGO_HTTP_CAINFO` с тем же
локальным bundle. Сертификат не входит в исходники, и TLS не отключается.
