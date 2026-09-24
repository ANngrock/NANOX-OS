# NANOX-OS

Самостоятельная ОС с собственным ядром, системными сервисами и встроенным
Cognitive Core. Архитектура и план этапов — [ARCHITECTURE.md](ARCHITECTURE.md).

Текущее состояние: этапы **M0 «Воспроизводимый стенд»** и **M1 «Собственная
загрузка и ядро»** выполнены, этап **M2 «Изоляция, потоки и IPC»** — частично.
Собственный UEFI-загрузчик (C17, PE/COFF) проверяет и загружает ядро
(freestanding C17 + ASM, ELF64) и initramfs (cpio newc) и передаёт boot info
1.1. Ядро ставит свои GDT/TSS/IDT, стеки со страницами-ограничителями,
физический allocator, свои таблицы страниц (W^X, physmap), таймер local APIC
и выдаёт проверяемые отчёты о panic и исключениях. С M2 ядро запускает
программы из initramfs в ring 3, каждую в своём адресном пространстве,
переключает задачи вытесняющим планировщиком по таймеру и передаёт между ними
сообщения и handle с уменьшенными правами. Проверены критерии M2 1–3;
критерии 4–6 (проверка аргументов syscall, изоляция памяти приложений,
Sovereign) реализованы в коде частично и тестами пока не проверены — см.
[docs/m2-kernel.md](docs/m2-kernel.md).

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
- [docs/m2-kernel.md](docs/m2-kernel.md) — решения M2: адресное пространство
  задачи, планировщик, ABI системных вызовов, handles и права, IPC,
  пользовательские программы, сценарии и что ещё не проверено.

## Структура

```text
abi/nanox/        общие форматы: boot info, манифест, коды диагностики, ABI системных вызовов
boot/uefi/        UEFI-загрузчик
kernel/           ядро: arch/x86_64 (вход, GDT/IDT, исключения, таймер, переключение
                  контекста, link map), mm (allocator, таблицы страниц, адресные
                  пространства, доступ к памяти пользователя), obj (handles, IPC, объекты),
                  задачи и планировщик, системные вызовы, тестовые режимы M2, initramfs, panic
user/             программы ring 3: rt (вход, обёртки syscall), test (тестовые программы M2)
initrd/           содержимое initramfs (программы из user/ добавляются в bin/ при сборке)
lib/              freestanding-код загрузчика, ядра и программ: serial, printf, SHA-256,
                  mem*, план загрузки ELF
tools/image/      детерминированные писатели образа GPT + FAT32 и initramfs
tools/bench/      конфигурация QEMU, harness, символизация, проверка GDB
tools/doctor.py   проверка окружения по toolchain.lock
tests/host/       host-тесты (C и Python)
tests/qemu/       сценарии стенда
```
