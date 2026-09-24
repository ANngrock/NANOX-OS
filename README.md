# NANOX-OS

Самостоятельная ОС с собственным ядром, системными сервисами и встроенным
Cognitive Core. Архитектура и план этапов — [ARCHITECTURE.md](ARCHITECTURE.md).

Текущее состояние: этап **M0 «Воспроизводимый стенд»**. Есть собственный
UEFI-загрузчик (C17, PE/COFF), минимальное ядро (freestanding C17 + ASM,
ELF64), которое проверяет переданный boot info и сообщает вердикт,
детерминированная сборка загрузочного образа и headless-стенд на QEMU.
Ядро пока не управляет памятью, прерываниями и задачами — это M1 и дальше.

## Быстрый старт

Требования (эталонная среда Ubuntu 24.04):

```sh
sudo apt-get install clang lld qemu-system-x86 ovmf mtools python3 make gdb
```

Затем из чистого checkout — одна последовательность:

```sh
make doctor && make && make test
```

- `make doctor` сверяет версии инструментов и хеши прошивки с `toolchain.lock`;
- `make` собирает `out/BOOTX64.EFI`, `out/kernel.elf`, `out/nanox.img`;
- `make test` запускает host-тесты и все сценарии QEMU; каждый запуск
  записывается в `out/runs/<время>-<сценарий>/record.json`.

Прочие цели: `make run` (одна загрузка с выводом serial), `make debug` +
`gdb -x tools/gdb/nanox.gdb` (отладка), `make repro-check` (побайтовая
воспроизводимость). Окружение Nix описано в `flake.nix`, но ещё не проверено
(см. [docs/m0-bench.md](docs/m0-bench.md#3-toolchain)).

## Документация

- [docs/m0-bench.md](docs/m0-bench.md) — конфигурация QEMU/UEFI, toolchain,
  harness, маркеры и коды выхода, запись запусков, воспроизводимость, GDB.
- [docs/boot-info.md](docs/boot-info.md) — интерфейс boot info 1.0, структура
  памяти, правила ошибок загрузчика и ядра.

## Структура

```text
abi/nanox/        общие для загрузчика и ядра форматы: boot info, манифест, коды диагностики
boot/uefi/        UEFI-загрузчик
kernel/           ядро (arch/x86_64: точка входа и link map)
lib/              freestanding-код загрузчика и ядра: serial, printf, SHA-256, mem*
tools/image/      детерминированный писатель образа GPT + FAT32
tools/bench/      конфигурация QEMU, harness, проверка GDB
tools/doctor.py   проверка окружения по toolchain.lock
tests/host/       host-тесты (C и Python)
tests/qemu/       сценарии стенда
```
