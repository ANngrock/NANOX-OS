# NANOX-OS

Самостоятельная ОС с собственным ядром, системными сервисами и встроенным
Cognitive Core. Архитектура и план этапов — [ARCHITECTURE.md](ARCHITECTURE.md).

Текущее состояние: этапы **M0 «Воспроизводимый стенд»** и **M1 «Собственная
загрузка и ядро»**. Собственный UEFI-загрузчик (C17, PE/COFF) проверяет и
загружает ядро (freestanding C17 + ASM, ELF64) и initramfs (cpio newc) и
передаёт boot info 1.1. Ядро ставит свои GDT/TSS/IDT, стеки со
страницами-ограничителями, физический allocator, свои таблицы страниц
(W^X, physmap), таймер local APIC и выдаёт проверяемые отчёты о panic и
исключениях. Потоков, планировщика, user mode и IPC ещё нет — это M2.

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
- `make test` запускает host-тесты, все сценарии QEMU и проверку
  повторяемости; каждый запуск записывается в
  `out/runs/<время>-<сценарий>/record.json`.

Прочие цели: `make run` (одна загрузка с выводом serial), `make debug` +
`gdb -x tools/gdb/nanox.gdb` (отладка), `make repro-check` (побайтовая
воспроизводимость). Окружение Nix описано в `flake.nix`, но ещё не проверено
(см. [docs/m0-bench.md](docs/m0-bench.md#3-toolchain)).

## Документация

- [docs/m0-bench.md](docs/m0-bench.md) — конфигурация QEMU/UEFI, toolchain,
  harness, маркеры и коды выхода, запись запусков, воспроизводимость, GDB.
- [docs/boot-info.md](docs/boot-info.md) — интерфейс boot info 1.1, манифест
  образа, физическая структура памяти, правила ошибок загрузчика и ядра.
- [docs/m1-kernel.md](docs/m1-kernel.md) — решения M1: initramfs, стеки,
  GDT/IDT, отчёт об исключениях, allocator, адресное пространство ядра,
  таймер, сценарии и их проверка.

## Структура

```text
abi/nanox/        общие для загрузчика и ядра форматы: boot info, манифест, коды диагностики
boot/uefi/        UEFI-загрузчик
kernel/           ядро: arch/x86_64 (вход, GDT/IDT, исключения, таймер, link map),
                  mm (allocator, таблицы страниц), initramfs, panic
initrd/           содержимое initramfs
lib/              freestanding-код загрузчика и ядра: serial, printf, SHA-256, mem*
tools/image/      детерминированные писатели образа GPT + FAT32 и initramfs
tools/bench/      конфигурация QEMU, harness, символизация, проверка GDB
tools/doctor.py   проверка окружения по toolchain.lock
tests/host/       host-тесты (C и Python)
tests/qemu/       сценарии стенда
```
