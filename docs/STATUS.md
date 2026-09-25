# Статус проекта

Обновлено: 2026-09-25. Основной checkout: `/home/holod/src/NANOX-OS` в WSL Ubuntu;
из Windows: `\\wsl.localhost\Ubuntu\home\holod\src\NANOX-OS`.
**M0 завершён** для указанного QEMU/OVMF-профиля. Исходный OneDrive-каталог
сохранён как входной снимок; текущие файлы и артефакты находятся в Linux checkout.

## Git-базис

- **Каноническая линия — Rust** (`AGENTS.md`, ADR-0001). Реализация M0
  закоммичена в ветке `codex/m0`: commit
  `b8913fa5a89a288347deae389f53d7f2b4a9261b` «feat(m0): implement Rust UEFI
  boot path and replay harness», родитель `c5c3d7f`. Ветка опубликована как
  `origin/codex/m0` на тот же commit.
- Страховочная локальная ветка `backup/m0-snapshot-2026-09-24` (`c8eba06`)
  хранит более ранний снимок того же M0; удалять её не требуется.
- `origin/main` (`152a67a` на последнем fetch 2026-09-24) — **расходящаяся
  неканоническая C17/ASM-линия** от общего предка `c5c3d7f`. Она противоречит
  неизменяемым требованиям Rust и не является продолжением этой реализации;
  её код не сливать и не переносить напрямую. Каталог переносимых из неё
  тестов и негативных сценариев: `docs/porting-from-c.md` в ветке
  `origin/claude/slack-session-ru31dg` (`d0d493a`); в эту линию он ещё не
  перенесён. `origin/claude/m2-wip` — старая C-WIP-ветка.
- Способ интеграции канонической Rust-линии с default-веткой `main` ещё не
  завершён; конфликты не разрешаются автоматически.
- Detached worktree в `out/clean-checkout-*` созданы проверками чистого
  checkout; их метаданные в `.git/worktrees` сохраняются.

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

## Повторная проверка 2026-09-24

Выполнена Codex на незакоммиченном дереве до commit `b8913fa`; в RunRecord
записаны commit `c5c3d7f`, `dirty: true`, source manifest
`def74b83c8c47945d4f958a37f3e08a7c1147081235f9e85aa1e44a8fac05b01`.
Это дерево не тождественно `b8913fa`: в `Cargo.toml`, `kernel/Cargo.toml`
и `kernel/linker.ld` при commit удалены финальные пустые строки и mode
изменён `100755 → 100644`. Проверка именно `b8913fa` — в следующем разделе.

- `nix develop --command cargo xtask test --replay`: 31 host-тест и семь
  QEMU-сценариев удовлетворили ожидания (PASS, три LOADER_ERROR, FAIL,
  PANIC, TIMEOUT). Итог: `out/runs/1790263663411034980-1071-suite/suite.json`.
- Replay PASS: raw serial, guest verdict, итоговые disk и VARS равны —
  `out/runs/1790263782429596132-1071-pass-replay-play/replay-comparison.json`.
  Replay FAIL: `out/runs/1790263822938643811-1071-kernel-fail-replay-play/replay-comparison.json`.
- `nix develop --command cargo xtask reproduce-build`: EFI и ELF двух сборок
  совпали с хешами выше; `disk_image_equality_tested: false`. Запись:
  `out/runs/1790263878934562544-908-reproduce-build/record.json`.
- `cargo xtask doctor` без failures (`out/doctor.json`); `cargo fmt --all -- --check`,
  `python3 tests/fixtures/generate.py --check`, `git diff --check` прошли.

## Проверка чистого checkout `b8913fa` (2026-09-25)

Выполнена Codex из чистого detached checkout
`out/review-clean-b8913fa-20260925-codex/` в WSL Nix devShell. Пути ниже
относительно этого каталога.

- `cargo xtask doctor`: `failures: []`, машина `pc-q35-9.2` (`out/doctor.json`).
  `cargo fmt --all -- --check` прошёл; `python3 tests/fixtures/generate.py --check`:
  6 golden fixtures совпали. 31 host-тест (6 + 16 + 9) прошёл в составе
  `test --replay`: `out/runs/1790303972939763602-363692-suite/host-tests.log`.
- `cargo xtask test --replay`: семь QEMU-сценариев удовлетворили ожидания
  (PASS, три LOADER_ERROR, FAIL, PANIC, TIMEOUT):
  `out/runs/1790303972939763602-363692-suite/suite.json`.
- Replay PASS и FAIL: raw serial, guest verdict, итоговые disk и VARS равны —
  `out/runs/1790304090391715258-363692-pass-replay-play/replay-comparison.json`,
  `out/runs/1790304129396745847-363692-kernel-fail-replay-play/replay-comparison.json`.
- `cargo xtask reproduce-build`: commit `b8913fa`, `dirty: false`, source
  manifest `d3a64f3cc0698f3a61e3554cbdfe7c76bc6e40d13fd97bf2bc65d0d5cad587b9`;
  EFI `3032af43e8da464572a4dbce68ebde6f405ca9dfdcc1b4f8272aaec29e3be719` и
  ELF `496ef48df665ff041324138f8fef156fb1dbf496b9f8aca1eae80c8824477e2a`
  двух сборок равны и совпадают с прежними; `disk_image_equality_tested: false`:
  `out/runs/1790304160434397777-366878-reproduce-build/record.json`.
- Clippy (диагностически): те же 12 ошибок, см. «Известный долг».
- Повторно, тем же checkout: `nix develop --command bash -c 'set -e; cargo xtask doctor;
  cargo fmt --all -- --check; python3 tests/fixtures/generate.py --check;
  cargo xtask test --replay'` — exit 0; 31 host-тест
  (`out/runs/1790334202426528211-733761-suite/host-tests.log`), семь
  сценариев (`out/runs/1790334202426528211-733761-suite/suite.json`), replay
  PASS и FAIL совпали
  (`out/runs/1790334318454873850-733761-pass-replay-play/replay-comparison.json`,
  `out/runs/1790334358894525536-733761-kernel-fail-replay-play/replay-comparison.json`).

## GDB

Программная точка останова проверена: `cargo xtask debug` (запуск
`out/runs/1790187734466547118-784-pass-debug/`, указан в
`software-breakpoint/run-dir.txt`), GDB с
`set breakpoint auto-hw off` и `break kernel_main` остановился в
`nanox_kernel::kernel_main+19`, затем гость завершился с кодом 33 (PASS).
Вывод: `out/debug-verification-1790162515014380353/software-breakpoint/gdb.stdout`,
код возврата GDB 0 (`gdb-returncode.txt`).
Аппаратная `hbreak` в этом профиле не поддерживается. Reverse debugging не
заявляется. Пункт закрыт и M1 не блокирует.

## Известный долг

- `cargo clippy --locked --package boot-protocol --package xtask --all-targets -- -D warnings`
  не проходит: lints `manual_div_ceil` и `manual_is_multiple_of` в
  `crates/boot-protocol`; из-за остановки xtask не получил полного анализа.
  Это не gate M0 и не ошибка сборки/тестов.
- `docs/specs/machine-profile.toml` входит в source fingerprint, но не
  читается `cargo xtask doctor`/runner; имя машины можно переопределить
  `NANOX_QEMU_MACHINE` без сверки с профилем. Фактические tool/firmware hashes
  при этом записываются в каждый RunRecord и сверяются replay preflight.
- Программная точка останова GDB на `b8913fa` не перепроверялась; прежнее
  доказательство — раздел «GDB».

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

Отдельными задачами, по одной:

1. Устранить Clippy-долг: `cargo clippy --locked --package boot-protocol
   --package xtask --all-targets -- -D warnings` проходит без ошибок.
2. Полная сверка runtime-профиля с `docs/specs/machine-profile.toml`: все
   исполняемые поля профиля, в том числе machine, версия и патч QEMU, CPU,
   vCPU, RAM, accelerator, RTC, network, display, boot controller, serial,
   debug exit, timeout, Rust, firmware, хеши CODE/VARS, firmware mapping,
   replay-режим и inputs, сверяются `doctor`/runner;
   расхождение, в том числе через `NANOX_QEMU_MACHINE`, — ошибка.
   Негативные host-тесты на несовпадение и повреждённый профиль.
3. Отдельный M1-контракт в `docs/specs/` до любого кода allocator: layout
   памяти, исключаемые диапазоны (kernel, handoff, page tables, stack,
   reserved/device memory), источник таймера, порядок блокировок, входы,
   выходы и негативные случаи. Физический allocator реализуется только после
   принятия контракта.

## Правило дальнейших обновлений

Для выполненного критерия записывать ревизию/хеш исходников, точную команду, окружение, вердикт и путь к RunRecord. Документация о будущей функции не является доказательством её реализации.
