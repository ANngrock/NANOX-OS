# Статус проекта

Обновлено: 2026-09-23. Основной checkout: `/home/holod/src/NANOX-OS` в WSL Ubuntu;
из Windows: `\\wsl.localhost\Ubuntu\home\holod\src\NANOX-OS`.
Ветка `codex/m0`, исходный HEAD `c5c3d7f608124fa40602f4404acff45b7e719714`.
Реализация находится в рабочем дереве; push не выполнялся. **M0 завершён**
для указанного QEMU/OVMF-профиля. Исходный OneDrive-каталог сохранён как входной
снимок; текущие файлы и артефакты находятся в Linux checkout.

## Выполнено

- Собственный Rust `no_std` UEFI loader загружает и валидирует ELF,
  создаёт page tables, завершает Boot Services и передаёт BootInfo своему ядру.
- Ядро проверяет BootInfo, CR3, данные/BSS, выводит serial report и завершает
  test profile через `isa-debug-exit`. Обычный профиль остаётся в idle.
- Nix/Rust/Cargo locks, GPT/FAT32 файловый образ, команды doctor/build/run/test,
  reproduce-build, replay и debug; полный RunRecord с исходными inputs.
- Семь QEMU-сценариев различают PASS, три ошибки loader, FAIL ядра, panic и
  timeout. 31 host-тест проверяет ELF/BootInfo и harness.

## Не выполнено

- M1–M10 ещё не реализованы: нет полноценного allocator, threads/userspace,
  IPC, сети, ИИ или self-hosting.
- Проверен QEMU-стенд; загрузка на физическом оборудовании не выполнялась.
- Reverse debugging и replay произвольных устройств не заявляются.

## Доказательства финального профиля

- Из чистого detached worktree, созданного на локальном verification commit
  `634920e3f4e2f940b546e091a739351bf712c6fb` без изменения основной ветки,
  выполнено `nix develop --command bash -c 'cargo xtask doctor && cargo xtask build && cargo xtask run'`.
  Exit 0, исходный Git status пуст; EFI, ELF и весь disk image побайтно совпали
  с основной сборкой. Отчёт:
  `out/clean-checkout-1790162485753406810/verification.json`.
- `nix develop --command cargo xtask reproduce-build`: две свежие сборки
  дали одинаковые EFI `3032af43e8da464572a4dbce68ebde6f405ca9dfdcc1b4f8272aaec29e3be719`
  и ELF `496ef48df665ff041324138f8fef156fb1dbf496b9f8aca1eae80c8824477e2a`.
  Запись: `out/runs/1790162469000326703-54688-reproduce-build/record.json`.
- `nix develop --command cargo xtask test --replay`: 31 host-тест и семь QEMU
  сценариев прошли. PASS exit 33, три loader reject exit 35 без входа в kernel,
  kernel FAIL exit 35, panic exit 35 и намеренный timeout. Итог:
  `out/runs/1790162453428181691-53812-suite/suite.json`;
  сырой serial, stderr, argv, входные hashes и исходники — в каждом RunRecord.
- Настоящий `icount` record/replay: PASS сохранил raw serial, guest verdict и
  exit 33; FAIL сохранил raw serial, verdict и exit 35. Итоговые VARS и disk
  байт-в-байт равны. Отчёты:
  `out/runs/1790162576333714864-53812-pass-replay-play/replay-comparison.json`
  и `out/runs/1790162617394717144-53812-kernel-fail-replay-play/replay-comparison.json`.
  Изменённый ELF отвергнут до QEMU:
  `out/runs/1790162617394717144-53812-kernel-fail-replay-play/replay-rejection.json`.
- `python3 tests/fixtures/generate.py --check`: 6 golden fixtures совпали.
  `cargo fmt --all -- --check` и `git diff --check` прошли.
- `out/doctor.json`: prerequisites совпадают. Закреплённые Rust 1.90.0,
  QEMU 9.2.4 с патчем `nanox-replay-exit-v1`, OVMF 202411, `pc-q35-9.2`.
  SHA-256 QEMU: `17aa3a6374281ce4e850536900ea7e91f5d1ea1b7891cb80e35bff772b16d6cd`.
  Source manifest финального replay:
  `9a8cfc4c408f58f844c4e50d0a49c2c7b8d241b759360760e4e989eaa669a4b9`.
  Обычный image `out/nanox.img` SHA-256
  `27cda63c50eda82217c2384ed0ae38de4a75e681ba5990d4b047288acae7da06`.

## Ранние проверки и диагностические неуспехи

- `out/runs/1790161673253865458-26670-pass-boot-test/`: реальная загрузка,
  BootInfo validated, PASS, raw exit 33.
- `out/runs/1790161667684804368-26670-suite/suite.json`: все семь обычных
  сценариев прошли, но общий suite красный из-за потери exit code в upstream replay.
- `out/runs/1790161738739460347-28279-reproduce-build/`: две чистые сборки
  EFI/ELF побайтно равны.
- `out/runs/1790161777762347455-26670-pass-replay-play/`: raw serial,
  итоговые VARS и disk совпали; replay exit 0 вместо 33. Не считается успехом.

## Следующее действие

M1: физический allocator, исключающий kernel, handoff, page tables и
reserved/device memory. Перед этим завершить отдельную интерактивную проверку
GDB breakpoint; команда `debug` реализована, но начальный probe столкнулся с
ограничением hardware breakpoint GDB stub. Результат следует добавить сюда.

## Правило дальнейших обновлений

Для выполненного критерия записывать ревизию/хеш исходников, точную команду, окружение, вердикт и путь к RunRecord. Документация о будущей функции не является доказательством её реализации.
