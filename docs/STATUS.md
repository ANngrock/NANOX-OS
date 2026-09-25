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
- Clippy (диагностически): 12 ошибок в boot-protocol; исправлено в «Срез 1: Clippy».
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

## Срез 1: Clippy (2026-09-25)

На `b8913fa` без исправлений `cargo clippy --locked --package boot-protocol
--package xtask --all-targets -- -D warnings` давал 13 ошибок: 11
`manual_is_multiple_of` и 1 `manual_div_ceil` в `crates/boot-protocol`, 1
`unused_unit` в `tools/xtask/src/build.rs` (ранее скрыт остановкой на
boot-protocol). Исправлено заменой на `is_multiple_of`/`div_ceil` и `{}`;
для единообразия так же заменены три однотипные проверки выравнивания в
`validate_reserved_ranges`/`validate_load_segments`. Делители — ненулевые
константы или `stride >= 40`, проверенный раньше в том же условии, поэтому
результаты проверок не меняются.

Проверка в основном checkout (`b8913fa` + рабочее дерево, `dirty: true`),
каждая команда ниже выполнена в `nix develop` по очереди (локальный
неотслеживаемый сценарий `out/slice1-checks.sh`); логи и коды выхода —
`out/slice1-clippy-20260925T113637Z/`:

- clippy `-D warnings` для обоих пакетов, `cargo fmt --all -- --check`,
  `python3 tests/fixtures/generate.py --check`, `cargo xtask doctor`,
  `git diff --check` — exit 0.
- `cargo xtask test --replay` — exit 0: 31 host-тест (6 + 16 + 9) и семь
  QEMU-сценариев (`out/runs/1790336200760340468-769146-suite/suite.json`);
  replay PASS и FAIL: raw serial, verdict, disk и VARS равны
  (`out/runs/1790336319140544651-769146-pass-replay-play/replay-comparison.json`,
  `out/runs/1790336358631110420-769146-kernel-fail-replay-play/replay-comparison.json`).
- `cargo xtask reproduce-build` — две сборки равны, source manifest
  `8c4ad81883ed60ec2c47dfc1670ec84e39a0f19648380619c9952ba573be846e`:
  `out/runs/1790336384168091360-772189-reproduce-build/record.json`.
  **Хеши бинарников изменились** относительно `b8913fa`: EFI
  `64b3716063f092e1e264c509399fcff3c198a740e10cec8acd5657c2449f3098`
  (было `3032af43…e719`), ELF
  `1d5df10d5dcbbb53a92a0991a3d4e94c85256b5584b1cac37754067faeb67348`
  (было `496ef48d…7e2`). `boot-protocol` компилируется в loader и kernel;
  другой исходный код даёт другой машинный код. Поведение подтверждено
  тестами и QEMU-сценариями выше, не сравнением бинарников.

## Срез M5-1: сетевые кодеки (2026-09-25)

По указанию пользователя M5 начат раньше M1–M4. **M5 начат, не завершён:**
ни один критерий M5 в ROADMAP не выполнен. Контракт и зависимости —
[M5-NETWORK](specs/M5-NETWORK.md).

- `crates/net-wire`: `no_std`, без `alloc`, `#![forbid(unsafe_code)]`, без
  сторонних crates. Кодеки Ethernet II, ARP (Ethernet/IPv4), IPv4, ICMP
  echo, UDP; RFC 1071 checksum. IPv4 options отдаются сырыми, фрагменты
  отвергаются (`Fragmented`), сборки нет. Добавлен в workspace и в
  host-тесты `cargo xtask test`.
- `crates/net-wire/tests/wire.rs`: 14 тестов — round trip, каждое правило
  политики отдельным негативным случаем, все усечения, 20 000
  псевдослучайных входов без panic.

Проверка в основном checkout (`b8913fa` + рабочее дерево, `dirty: true`),
каждая команда в `nix develop`; логи и коды выхода — повторный прогон после
ревью (добавлены QinQ, групповые MAC/IPv4 отправителя ARP в разборе и
эмиссии) `out/m5-1-checks-20260925T132032Z/`, первый —
`out/m5-1-checks-20260925T130157Z/`:

- `cargo clippy --locked --package boot-protocol --package net-wire
  --package xtask --all-targets -- -D warnings`,
  `cargo build --locked --package net-wire --target x86_64-unknown-none`,
  `cargo fmt --all -- --check`, `python3 tests/fixtures/generate.py --check`,
  `cargo xtask doctor`, `git diff --check` — exit 0.
- `cargo xtask test --replay` — exit 0: 45 host-тестов (6 + 16 + 14 + 9) и
  семь QEMU-сценариев M0 без регрессий
  (`out/runs/1790342434069814160-858686-suite/suite.json`); replay PASS и
  FAIL совпали
  (`out/runs/1790342549482249796-858686-pass-replay-play/replay-comparison.json`,
  `out/runs/1790342588303281291-858686-kernel-fail-replay-play/replay-comparison.json`).
- В QEMU сетевой код не исполняется: в guest нет драйвера, профиль M0
  отключает сеть.
- Исправления по ревью (часть A пакета M5-2): описание в `lib.rs` о
  заимствовании только данных переменной длины; тесты broadcast
  IPv4-источника (отвергается) и broadcast-назначения (принимается), round
  trip ICMP echo reply, в том числе с пустыми данными.

## Срез M5-2: DNS-кодек (2026-09-25)

Модуль `net_wire::dns` (`crates/net-wire/src/dns.rs`), контракт —
[M5-NETWORK §5](specs/M5-NETWORK.md). Stub A/IN: запрос с RD; проверка ID,
QR/opcode/QDCOUNT, лимита записей, эха вопроса до RCODE, TC и RCODE.
Результат — только A-записи секции Answer, достижимые от QNAME через не
более 8 CNAME; Authority/Additional проходят те же проверки RDATA A/CNAME,
но в результат не попадают. Циклы и конфликты CNAME, переполнение
выходного массива (`OutputFull`; ёмкость проверяется до записи, массив не
меняется), минимум TTL по CNAME и A, лимит имени 255 байт, указатели
сжатия только назад. 14 тестов в `crates/net-wire/tests/dns.rs`.
M5 по-прежнему только начат: транспорт DNS, повторы и таймауты — в
сетевом сервисе после M1–M4.

Проверка в основном checkout (`b8913fa` + рабочее дерево, `dirty: true`),
те же команды, что для M5-1, в `nix develop`. Повторный прогон после
ревью (ёмкость `OutputFull` до записи, RDATA в Authority/Additional) —
`out/m5-1-checks-20260925T140324Z/`, первый —
`out/m5-1-checks-20260925T134624Z/`: clippy `-D warnings`, `net-wire` для
`x86_64-unknown-none`, fmt, fixtures, doctor, `git diff --check` — exit 0;
`cargo xtask test --replay` — exit 0: 59 host-тестов (6 + 16 + 14 + 14 + 9),
семь QEMU-сценариев (`out/runs/1790345006243449378-900379-suite/suite.json`),
replay PASS и FAIL совпали
(`out/runs/1790345120357154504-900379-pass-replay-play/replay-comparison.json`,
`out/runs/1790345159487682655-900379-kernel-fail-replay-play/replay-comparison.json`).

## Известный долг

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

По указанию пользователя приоритет — M5, затем остальные этапы. Срез M5-1
принят; M5-2 (DNS) ожидает ревью. Следующий host-проверяемый шаг M5 без
M1–M4 — ARP neighbor cache фиксированной ёмкости с внешним временем. Затем
M1–M4, интеграция M5 и M6–M10 по ROADMAP.

1. ~~Clippy-долг~~ — выполнено и принято, см. «Срез 1: Clippy».
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
