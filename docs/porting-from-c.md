# Перенос проверок из C-реализации: каталог предложений

## 0. Назначение и статус

- **Что это.** Каталог проверок, сценариев, отрицательных контролей, соглашений
  стенда и подводных камней C/ASM-реализации NANOX-OS, которые имеет смысл
  перенести в Rust-реализацию. Это **не требование** и не спецификация: каждый
  пункт — предложение, решение о переносе принимают Codex и владелец проекта.
- **База.** Ревизия `152a67a` (`origin/main`, merge PR #3). Все ссылки на файлы,
  сценарии и разделы документов относятся к этой ревизии, кроме помеченных
  префиксом `b8913fa:` (Rust, см. «Как проверить ссылки»). Где в C что-то только
  описано, но не выполнялось, это сказано явно.
- **Статус C-ветки.** Разработка на C остановлена; C-реализация **не
  каноническая**. Каноническая база — Rust-реализация (Rust stable, `no_std`
  загрузчик и ядро, `cargo xtask`, Nix, QEMU 9.2.4, `pc-q35-9.2`, ROADMAP
  M0–M10), сейчас на M0. Этапы C (M0–M5) — это этапы ARCHITECTURE.md §15, а не
  ROADMAP Rust-реализации.
- **Rust-этапы.** Колонку «Rust-этап» заполнил Claude по ROADMAP
  Rust-реализации (`b8913fa:docs/ROADMAP.md`, ветка `codex/m0`, коммит
  `b8913fa5a89a288347deae389f53d7f2b4a9261b`); сопоставление **требует
  подтверждения Codex**. В ячейке — этап ROADMAP (M0–M10) и состояние в Rust
  на `b8913fa`: «есть в Rust M0», «частично есть: <чего не хватает>» или «нет».
  Сопоставления, в которых автор не уверен, помечены «(предварительно —
  подтвердить Codex)». Rust сейчас на M0, поэтому для этапов M1 и дальше
  состояние везде «нет».
- **Reviewer:** Codex.
- **Как проверить ссылки.** Пути в обратных кавычках относятся к разным
  ревизиям в зависимости от класса:
  - обычный путь (`tools/bench/harness.py`) — C-реализация, существует в
    `152a67a` (`git cat-file -e 152a67a:<путь>`);
  - путь с префиксом `b8913fa:` (`b8913fa:tools/xtask/src/runner.rs`) —
    Rust-реализация, существует в `b8913fa`
    (`git cat-file -e b8913fa:<путь>`); ссылки на строки ведут на GitHub
    по полному хешу этого коммита;
  - имя сценария (`normal`, `m4-crash`) есть в `tests/qemu/scenarios.json`
    на `152a67a`. Имена Rust-сценариев (pass, kernel-fail и т. п. из
    `b8913fa:tools/xtask/src/main.rs`) пишутся без обратных кавычек.

  Исключения — пути, которых в дереве `152a67a` **намеренно нет**:
  - артефакты сборки и пути внутри initramfs: `out/...`, `bin/...`,
    `BOOTX64.EFI`, `kernel.elf`, `initrd.img`, `nanox.img`, `data.img`,
    `KERNEL.ELF`, `INITRD.IMG`, `OVMF_VARS.fd`;
  - имена файлов и подкаталогов в каталоге запуска C (`out/runs/<UTC>-<сценарий>/`
    на `152a67a`): `record.json`, `serial.log`, `qemu-output.log`,
    `bridge-trace.jsonl`, `bridge-wire.log`, `boot-<n>/`,
    `k<K>-<политика>/{crash,check}/`, `summary.json`, `repeat.json`, `gdb.log`;
  - имена файлов в каталоге запуска Rust (`out/runs/<run-id>/` на `b8913fa`,
    `b8913fa:docs/specs/TESTING-REPLAY.md` §5): `record.json`, `inputs.json`,
    `serial.bin`, `serial.txt`, `initial.img`, `initial-code.fd`,
    `initial-vars.fd`, `BOOT.CFG`, `suite.json`, `replay-comparison.json`;
  - корневые файлы Rust-реализации, которых в C нет: `flake.lock` (в C не
    создан, раздел 5), `Cargo.lock`, `rust-toolchain.toml` — существуют в
    `b8913fa`;
  - не пути: имена веток (`origin/main`, `codex/m0`) и операций NCI
    (`task.spawn/terminate`).

Главные источники: `tests/qemu/scenarios.json` (54 сценария),
`tools/bench/harness.py`, `tools/bench/qemu.py`, `tools/bench/storecheck.py`,
`tools/bench/gdb_check.py`, `tools/repro_check.py`, `tests/host/`,
`docs/m0-bench.md`, `docs/boot-info.md`, `docs/m1-kernel.md` …
`docs/m5-net.md`, `ARCHITECTURE.md` §15.

---

## 1. Общие соглашения стенда

### 1.1 Serial-маркеры

Источник: `docs/m0-bench.md` §6 «Serial-маркеры», `tools/bench/harness.py`
(`serial_lines`, `classify`).

- Маркер — строка, которая **начинается** с `NANOX: `. Перед сравнением
  удаляются `\r` и ANSI-последовательности (вывод OVMF): `ANSI_RE` в
  `tools/bench/harness.py`. Проверено тестом `test_marker_must_start_line` в
  `tests/host/test_harness.py`.
- Итоговые маркеры: `NANOX: TEST PASS`, `NANOX: TEST FAIL <причина>`,
  `NANOX: PANIC <сообщение>`, `NANOX: LOADER ERROR <ИМЯ> (<код>): …`,
  `NANOX: EXCEPTION …` + `NANOX: REGS …` + `NANOX: BACKTRACE <n> <адрес>`,
  `NANOX: CRASH POINT …`.
- Вывод пользовательских задач ядро печатает как `NANOX: USER <имя>#<id>: …`;
  задача не может напечатать строку, начинающуюся с другого маркера
  (`docs/m0-bench.md` §6). **Предложение:** сохранить это свойство — иначе
  гостевой код может подделать `TEST PASS`.
- Обязательная последовательность для PASS: `NANOX: loader start` →
  `NANOX: loader exit_boot_services ok` → `NANOX: kernel_main` →
  `NANOX: bootinfo ok` → ровно одна `NANOX: TEST PASS` (`PASS_SEQUENCE` в
  `tools/bench/harness.py`).

Переносимость: **высокая** (формат строк не зависит от языка). Имена маркеров
можно менять, но правило «маркер только с начала строки, гость не может его
подделать» стоит оставить.

**Rust-этап:** M0 — частично есть: маркеры вида `NANOX:<КОМПОНЕНТ>:<СОБЫТИЕ>`
ищутся как подстрока в любом месте serial-потока ([`b8913fa:tools/xtask/src/runner.rs` L25–27](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L25-L27)),
требования «с начала строки» и порядка нет (см. 1.3). Свойство «гость не
подделает маркер» становится существенным с появлением пользовательских
задач — M2 (предварительно — подтвердить Codex).

### 1.2 Коды выхода (isa-debug-exit)

Источник: `docs/boot-info.md` §8, `docs/m0-bench.md` §6 «Коды выхода».
Устройство `isa-debug-exit,iobase=0xf4,iosize=0x01`; статус процесса QEMU =
`(значение << 1) | 1`.

| Значение в порт `0xF4` | Статус QEMU | Смысл | Класс harness |
| ---: | ---: | --- | --- |
| `0x10` | 33 | TEST PASS | `PASS` |
| `0x11` | 35 | TEST FAIL | `test_fail` |
| `0x12` | 37 | PANIC | `panic` |
| `0x13` | 39 | LOADER ERROR | `loader_error` |
| `0x14` | 41 | необработанное исключение CPU (с M1) | `exception` |
| `0x15` | 43 | остановка в точке сбоя (с M4, только `m4-*` с `nanox.m4.crash`) | `crash_point` |
| — | 0 | сброс/тройная ошибка при `-no-reboot` | `unexpected_exit` |
| — | нет | процесс остановлен harness | `timeout` |

Коды нечётные и ≠ 0/1, поэтому их нельзя спутать ни с нормальным завершением
QEMU, ни с его собственной ошибкой. Переносимость: **высокая**.

**Rust-этап:** M0 — частично есть: устройство то же (`iosize=4`), значения
`0x10` → 33 и `0x11` → 35 (`b8913fa:docs/specs/TESTING-REPLAY.md` §4). FAIL,
panic и отказ загрузчика в Rust M0 все дают 35 и различаются только маркером
([`b8913fa:tools/xtask/src/runner.rs` L67–89](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L67-L89)); отдельных кодов для panic, loader
error, исключения CPU (M1) и точки сбоя (M4) нет.

### 1.3 Классы вердикта и правило «маркер + код»

Источник: `classify()` в `tools/bench/harness.py`, `docs/m0-bench.md` §6
«Правила вердикта», тесты `tests/host/test_harness.py`.

- **PASS** только если статус 33 **и** ровно одна строка `TEST PASS` **и**
  полная последовательность из 1.1 до неё **и** нет строк `TEST FAIL`,
  `PANIC`, `LOADER ERROR`, `EXCEPTION`.
- 35/37/39/41 требуют соответствующего маркера; 43 требует **ровно одну**
  строку `CRASH POINT` и отсутствие `TEST FAIL`/`PANIC`/`EXCEPTION`/`TEST PASS`.
- Несогласованность статуса и маркеров → `inconsistent` (например, 33 без
  `TEST PASS`, `TEST PASS` до `bootinfo ok`, два `TEST PASS`).
- Таймаут → `timeout`, даже если в логе есть `TEST PASS`.
- Любой другой статус → `unexpected_exit`.

Тесты, которые стоит перенести как есть (они проверяют правила, а не C-код):
`test_pass_requires_marker_and_exit`, `test_exit_33_without_marker_is_fail`,
`test_marker_without_exit_is_fail`, `test_pass_marker_with_timeout_is_fail`,
`test_pass_with_panic_line_is_fail`, `test_duplicate_pass_is_fail`,
`test_out_of_order_sequence_is_fail`, `test_pass_before_bootinfo_is_fail`,
`test_fail_panic_loader`, `test_other_exit_codes`,
`test_exception_exit_code` (все — `tests/host/test_harness.py`); для точки
сбоя — `test_crash_point`, `test_crash_exit_without_marker`,
`test_crash_marker_with_pass` (`tests/host/test_storecheck.py`).

Переносимость: **высокая**. Если в Rust-стенде вердикт считает `xtask`, это
чистая функция `(serial, exit_status, timed_out) → verdict`, её легко
покрыть теми же таблицами случаев.

**Rust-этап:** M0 («PASS, FAIL и timeout различаются автоматически») —
частично есть: нет проверки порядка маркеров и единственности PASS
([`b8913fa:tools/xtask/src/runner.rs` L29–64](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L29-L64): `classify` проверяет только
наличие пяти маркеров и статус 33). Есть: статус без маркеров никогда не PASS,
маркеры ошибок важнее PASS, таймаут важнее PASS, любая подстрока `PANIC`/`FAIL`
→ FAIL; тесты `exit_without_evidence_is_never_pass`,
`errors_override_success_markers`
([`b8913fa:tools/xtask/src/runner.rs` L599–615](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L599-L615)). Классы `exception` (M1) и
`crash_point` (M4) появятся на своих этапах.

### 1.4 Ожидания сценария (`expect`)

Источник: `docs/m0-bench.md` §6, `tests/qemu/scenarios.json`.

- `verdict`, `failure_class`, `loader_error` (имя, например `E_KERNEL_HASH`);
- `patterns` — регулярные выражения по serial-логу (многострочные,
  с обратными ссылками: `m2-user` сверяет счётчики ресурсов «до» и «после»
  через `\1`/`\2`); подстановки `{kernel_sha256}`, `{initrd_sha256}` — хеши
  артефактов, посчитанные на хосте (поэтому литеральные `{}` удваиваются);
- `exception` (`mnemonic`, `vector`, `error`, `cr2`, `rip_function`,
  `cr2_is_rip`) и `backtrace_functions` — символизация по `.symtab`
  (`tools/bench/elfsym.py`, адреса возврата — по `адрес − 1`);
- `interleave` (`pattern`, `groups`) — см. `m2-sched`;
- `bridge` (`ok`, `problems` — регулярные выражения, каждому должна
  соответствовать найденная host-проблема; так отрицательный контроль
  проверяет, что упала **именно та** проверка);
- `store`, `data_disk_unchanged`, `boots`, `after.corrupt`, `data_disk: "fresh"`,
  `kind: "crash-sweep"` (M4); `m5` (сервисы стенда M5).

Проверка самих сценариев: `test_required_scenarios_present`,
`test_scenario_patterns_are_valid` (`tests/host/test_harness.py`) — список
обязательных сценариев и компилируемость всех выражений. Переносимость:
**высокая**. Особо рекомендуется идея `expect.bridge.problems`: отрицательный
контроль должен падать по ожидаемой причине, а не по любой.

**Rust-этап:** M0 — частично есть: ожидание каждого из семи сценариев
зашито в код функции `expected` ([`b8913fa:tools/xtask/src/runner.rs` L67–89](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L67-L89)),
а не в декларативный файл сценариев; регулярных выражений по логу и
подстановок хешей нет. `bridge.problems` — M3, `store`/`boots` — M4.

### 1.5 Запись запуска (run record)

Источник: `docs/m0-bench.md` §7. Каталог `out/runs/<UTC>-<сценарий>/`:
`record.json` (схема `nanox.run-record.v1`), `serial.log`, `qemu-output.log`,
для M3 `bridge-trace.jsonl` и `bridge-wire.log`, для M4 `data.img`,
`boot-<n>/`, `k<K>-<политика>/{crash,check}/`.

Поля `record.json`:

| Поле | Содержимое |
| --- | --- |
| `scenario`, `description`, `started_utc`, `duration_s` | что и когда |
| `source.git_rev`, `source.git_dirty`, `source.git_dirty_paths` | ревизия и незакоммиченные изменения |
| `qemu.argv`, `qemu.command_line`, `qemu.timeout_s` | полная командная строка |
| `image_spec` | cmdline и fault injection сценарного образа |
| `artifacts.{image,loader_efi,kernel_elf,initrd,ovmf_code,ovmf_vars_template}` | путь, SHA-256, размер |
| `toolchain.profile`, `toolchain.lock_ok`, `toolchain.versions` | фактические версии и сверка с lock |
| `host` | ОС и версия Python |
| `result.exit_status`, `result.timed_out`, `result.killed_returncode` | исход процесса |
| `serial.sha256`, `serial.markers`, `serial.raw` | лог целиком |
| `verdict`, `failure_class`, `loader_error`, `reason` | вердикт |
| `exception`, `backtrace` | разобранный отчёт с символами |
| `bridge`, `data_disk` | M3/M4 |
| `expected`, `expectation_met`, `expectation_problems` | ожидание и расхождения |

Дополнительно `summary.json` прогона и `repeat.json` проверки повторяемости.
Копия `OVMF_VARS.fd` на каждый запуск свежая и удаляется; в записи остаётся
хеш шаблона. Переносимость: **высокая** (формат JSON, язык не важен).

**Rust-этап:** M0 («Сохранён полный RunRecord») — частично есть. Rust
пишет `out/runs/<run-id>/record.json` и `inputs.json`
([`b8913fa:tools/xtask/src/runner.rs` L311–403](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L311-L403)); схема —
`b8913fa:docs/specs/TESTING-REPLAY.md` §5. Сопоставление полей:

| Поле C `record.json` | Rust `record.json` на `b8913fa` | Состояние |
| --- | --- | --- |
| `scenario`, `started_utc`, `duration_s` | `scenario`, `mode`, `started_unix_ms`, `result.process.elapsed_ms` | есть |
| `source.git_rev`, `git_dirty`, `git_dirty_paths` | `source.commit`, `branch`, `dirty`, `manifest_sha256`, `patch_sha256`, `files` (хеш каждого входного файла, включая неотслеживаемые) — `b8913fa:tools/xtask/src/record.rs` | есть, подробнее, чем в C |
| `qemu.argv`, `qemu.timeout_s` | `invocation.argv`, `invocation.environment`, `invocation.timeout_seconds` | есть |
| `artifacts.*` (путь, SHA-256, размер) | `inputs` — SHA-256 `initial.img`, `initial-code.fd`, `initial-vars.fd`, `BOOTX64.EFI`, `KERNEL.ELF`, `BOOT.CFG`; копии файлов лежат в каталоге запуска | есть (без размеров) |
| `toolchain.profile`, `lock_ok`, `versions` | `tools`: версии rustc/cargo/QEMU/qemu-img/sgdisk/mtools, SHA-256 бинарника QEMU, хеши `flake.lock`, `Cargo.lock`, `rust-toolchain.toml` | частично: версии и хеши есть, итог сверки с закреплённым профилем (аналог `lock_ok`) в запись не попадает — его выдаёт только `doctor` в `out/doctor.json` |
| отдельного поля нет: параметры машины в C видны только в `qemu.argv`, канон — `docs/m0-bench.md` §2 | `machine`: machine type, CPU, vCPU, RAM, accelerator, RTC, сеть, контроллер диска, serial, `icount`, способ подключения прошивки | есть; см. ниже |
| `host` | — | нет |
| `result.*`, `verdict`, `failure_class`, `reason` | `result.verdict`, `result.reason`, `result.process` (`exit_code`, `timed_out`, `spawn_error`, `signal`) | есть |
| `serial.sha256`, `serial.markers`, `serial.raw` | `outputs.serial_sha256` + файлы `serial.bin`/`serial.txt` | частично: разобранного списка маркеров нет |
| `expected`, `expectation_met`, `expectation_problems` | `result.expectation_met` | частично: только флаг, без списка расхождений |
| `exception`, `backtrace`; `bridge`, `data_disk` | — | нет; этапы M1, M3, M4 |
| `summary.json`, `repeat.json` | `suite.json` (`cargo xtask test`), `replay-comparison.json` | есть аналог `summary.json`; `repeat.json` — нет (см. 1.6) |

**Учёт профиля машины.** Отдельного поля «версия/хеш machine profile» в
Rust-записи нет, но **отсутствие отдельного поля не означает отсутствия
доказательства**:

- параметры машины записываются в каждую запись напрямую: `profile()` →
  `inputs.json`/`record.json` поле `machine`
  ([`b8913fa:tools/xtask/src/runner.rs` L91–103](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L91-L103),
  [`b8913fa:tools/xtask/src/runner.rs` L151–164](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L151-L164), копирование в запись —
  [`b8913fa:tools/xtask/src/runner.rs` L360–363](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L360-L363)); хеши прошивки CODE/VARS — в
  `inputs`, бинарника QEMU — в `tools.qemu_sha256`;
- файл `b8913fa:docs/specs/machine-profile.toml` входит в манифест входов
  (`source.files`, `source.manifest_sha256`,
  [`b8913fa:tools/xtask/src/record.rs` L86–100](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/record.rs#L86-L100)), так что его изменение меняет
  хеш манифеста, а replay с другим манифестом отвергается
  ([`b8913fa:tools/xtask/src/runner.rs` L183–201](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L183-L201)); это соответствует
  ADR-0001 ([`b8913fa:docs/adr/0001-platform.md` L29–31](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/docs/adr/0001-platform.md#L29-L31): «versioned machine и
  firmware hashes», «RunRecord с source/toolchain/machine hashes»).

Ограничение: `doctor` **не сверяет** окружение с
`b8913fa:docs/specs/machine-profile.toml` — он проверяет версии QEMU и Rust,
наличие машины `pc-q35-9.2` (или значения `NANOX_QEMU_MACHINE`), прошивки как
файлы и lock-файлы, но не читает этот TOML и не сравнивает хеши прошивки с
`firmware_code_sha256`/`firmware_vars_sha256` из него
([`b8913fa:tools/xtask/src/build.rs` L24–128](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/build.rs#L24-L128)). Профиль в TOML — документ и вход
манифеста, а не исполняемая проверка. Аналог C-правила «профиль без
`status = verified` отвергается» (раздел 1.9) отсутствует.

### 1.6 Проверка повторяемости с маскированием (`harness.py repeat`)

Источник: `cmd_repeat`, `normalized_markers`, `VOLATILE_RE`,
`REGS_SCRATCH_RE`, `repeat_view` в `tools/bench/harness.py`;
`docs/m0-bench.md` §6; `Makefile` (цель `qemu-test`:
`harness.py repeat normal pagefault m2-sched --count 3`).

Сравниваются нормализованные маркеры N запусков по порядку. Маскируется:

| Что | Почему |
| --- | --- |
| `ticks=`, `tsc_delta=`, `lapic_per_10ms=` | измерения времени (M1) |
| `preempted=`, `preemptions=`, `switches=`, `latest_first=`, `earliest_last=` | где именно таймер вытеснил задачу (M2) |
| `r8`–`r11` **только** в строках `NANOX: REGS` | caller-saved регистры держат остатки прежних вызовов; в `pagefault` r9 содержал число тиков самопроверки таймера (29 или 30 в зависимости от хоста) |
| строки по выражению `repeat.unordered` сценария | порядок вывода вытесняемых задач; сравниваются как мультимножество (`m2-sched`: `^NANOX: USER `) |

Всё остальное — адреса, RIP, CR2, CR3, backtrace — обязано совпасть. Тесты:
`test_only_timing_fields_are_masked`, `test_scheduling_statistics_are_masked`,
`test_unordered_lines_compared_as_multiset`,
`test_scratch_registers_masked_in_reports_only`.

Переносимость: **средняя**. Идея переносится полностью; конкретный список
масок зависит от того, какие поля печатает Rust-ядро. Для M3–M5 повторяемость
в C **не настраивалась** (`boot_id`, тики, доля CPU меняются;
`docs/m3-core.md` §12).

**Rust-этап:** M1 (предварительно — подтвердить Codex: в ROADMAP нет
отдельного пункта; ближайший — «Время и внешние события имеют явные
abstraction boundaries и trace IDs»). Состояние: нет. В Rust M0 есть
другая проверка — record/replay с побайтным сравнением serial
(`b8913fa:docs/specs/TESTING-REPLAY.md` §1 п. 3); повтор N свежих загрузок с
маскированием (там же, п. 2) не автоматизирован.

### 1.7 Побайтовая воспроизводимость (`make repro-check`)

Источник: `tools/repro_check.py`, `docs/m0-bench.md` §5.

- Дерево (отслеживаемые и неигнорируемые файлы) копируется в два новых
  каталога с разными абсолютными путями; `make all` там и в рабочем дереве;
  требуется побайтовое совпадение `ARTIFACTS = ("BOOTX64.EFI", "kernel.elf",
  "initrd.img", "nanox.img", "data.img")` во всех трёх сборках. Отчёт —
  `out/repro-check.json`.
- Сравниваются **и готовые образы дисков** (`nanox.img` — GPT+FAT32 ESP,
  `data.img` — пустое хранилище), а не только ELF/EFI. Для этого образ пишет
  собственный детерминированный писатель (`tools/image/mkimage.py`:
  фиксированные GUID, метки времени из `SOURCE_DATE_EPOCH` или 1980-01-01),
  initramfs — `tools/image/mkinitrd.py` (сортировка, последовательные inode,
  uid/gid 0, фиксированные режимы).
- Отрицательный контроль при подготовке M0: сборка с пустым `REPRO_FLAGS` в
  двух путях дала разные `kernel.elf` (абсолютный путь в DWARF) — проверка
  такие различия видит.
- **Замечание для Rust:** по данным постановки задачи Rust-версия пока не
  сравнивает весь образ диска. Рекомендуется добавить в сравнение образ(ы)
  диска и initramfs, а отрицательным контролем проверить, что сборка из
  другого пути без remap-флагов (для Rust — `--remap-path-prefix`) даёт
  расхождение. Сам факт, что Rust-сборка без remap даёт разные бинарники,
  здесь не проверялся.
- **Rust-этап:** M0 («Проверена повторяемость бинарных сборок») —
  частично есть: `cargo xtask reproduce-build` собирает EFI/ELF дважды в
  свежих каталогах `target` и сравнивает хеши; в записи прямо стоит
  `disk_image_equality_tested: false`
  ([`b8913fa:tools/xtask/src/build.rs` L220–252](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/build.rs#L220-L252)). Обе сборки идут из одного
  checkout (другой абсолютный путь к исходникам не проверяется), отрицательного
  контроля нет. Однократное побайтное совпадение всего образа из чистого
  worktree записано в `b8913fa:docs/STATUS.md`, но это ручная проверка, а не
  часть `reproduce-build`.

Переносимость: **высокая**.

### 1.8 Автоматическая проверка GDB (`make debug-check`)

Источник: `tools/bench/gdb_check.py`, `tools/gdb/nanox.gdb`,
`docs/m0-bench.md` §8.

QEMU запускается с `-gdb tcp:127.0.0.1:<свободный порт> -S`; GDB в batch-режиме
ставит `hbreak kernel_main`, продолжает и проверяет: остановка в
`kernel_main`, `RIP` внутри `kernel_main`, по адресу из `RDI` лежит magic
boot info. Запись — `out/runs/<время>-gdb/record.json` и `gdb.log`.
Аппаратная точка останова выбрана потому, что загрузчик копирует ядро уже
после подключения GDB (программная под KVM была бы затёрта; под TCG работают
обе).

Переносимость: **средняя** — для Rust нужен неискажённый символ точки входа
(`#[no_mangle]` или проверка по демангленному имени) и знание, в каком
регистре передаётся boot info в Rust-ABI входа.

**Rust-этап:** M0 (`cargo xtask debug`) — частично есть: `debug --run ID`
запускает QEMU с `-S -gdb tcp:127.0.0.1:1234` на входах записанного запуска и
печатает команды GDB ([`b8913fa:tools/xtask/src/runner.rs` L305–307](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L305-L307),
[`b8913fa:tools/xtask/src/runner.rs` L354–358](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L354-L358)); автоматической проверки
остановки в `kernel_main` и содержимого boot info нет, а интерактивная
проверка точки останова, по `b8913fa:docs/STATUS.md`, ещё не завершена.

### 1.9 Прочие соглашения, которые стоит сохранить

| Соглашение | Источник | Зачем | Rust-этап |
| --- | --- | --- | --- |
| Единственный источник командной строки QEMU (`qemu.py print`), каждое отклонение от канона перечислено | `tools/bench/qemu.py`, `docs/m0-bench.md` §2 | сценарии не расходятся незаметно | M0 — есть в Rust M0: argv строит одна функция `qemu_args` и пишет в запись массивом ([`b8913fa:tools/xtask/src/runner.rs` L214–309](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L214-L309)); отдельного перечня отклонений от канона нет |
| `-no-user-config -nodefaults`, версионированный machine type, фиксированные CPU/RAM/SMP, `-rtc base=…,clock=vm` | `docs/m0-bench.md` §2 | детерминизм; фиксированная дата нужна и для срока сертификатов M5 (`docs/m5-net.md` §3) | M0 — частично есть: `-nodefaults`, `pc-q35-9.2`, фиксированные CPU/RAM/SMP и `-rtc base=…,clock=vm` есть ([`b8913fa:tools/xtask/src/runner.rs` L214–241](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/tools/xtask/src/runner.rs#L214-L241)); `-no-user-config` нет |
| Свежая копия `OVMF_VARS` на запуск; прошивка закреплена по SHA-256 | `docs/m0-bench.md` §2 | одинаковое начальное NVRAM | M0 — частично есть: VARS — свежий qcow2-оверлей над исходным шаблоном, хеши CODE/VARS в записи; `doctor` хеши прошивки с `b8913fa:docs/specs/machine-profile.toml` не сверяет (раздел 1.5) |
| `make doctor` сверяет версии и хеши с `toolchain.lock`, профиль без `status = verified` отвергается | `tools/doctor.py`, `toolchain.lock` | непроверенное окружение не выдаётся за проверенное | M0 — частично есть: `doctor` проверяет версии Rust/QEMU, sysroot, машину и наличие lock-файлов; статуса профиля и сверки хешей прошивки нет (раздел 1.5) |
| Неизвестное значение `nanox.test=` — `TEST FAIL`, а не молчаливый PASS | сценарий `unknown-test` | опечатка в сценарии не превращается в зелёный тест | M0 — нет: неизвестное значение `boot_epoch` в test-профиле даёт PASS ([`b8913fa:kernel/src/main.rs` L190–204](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/kernel/src/main.rs#L190-L204)) |
| Сторожевой таймер в прерывании печатает `TEST FAIL watchdog: …` раньше таймаута harness | `docs/m2-kernel.md` §2 | содержательный вердикт вместо `timeout`; зависание с запрещёнными прерываниями остаётся таймаутом | M1 (timer, «ошибки … диагностика») — нет (предварительно — подтвердить Codex) |
| Все ожидания в ядре ограничены числом опросов; сбой — `TEST FAIL <причина>`, не зависание | `docs/m1-kernel.md` §7 | зависание превращается в содержательный `TEST FAIL`, а не в `timeout` без причины | M1 («GDT/IDT, исключения, timer … работают») (предварительно — подтвердить Codex) — нет |
| Контроллер теста сверяет счётчики ресурсов (свободные страницы, таблицы, задачи, объекты, endpoint'ы) до и после | `docs/m2-kernel.md` §2, `docs/m3-core.md` §8 | утечки видны в каждом сценарии, а не в отдельном тесте | M1 (allocator, «Проверены … исчерпание памяти»), для задач и объектов — M2 (предварительно — подтвердить Codex) — нет |
| Итог host-проверок передаётся гостю (`session.close host_checks=ok\|fail`) и становится кодом выхода гостевого процесса → вердиктом ядра | `docs/m3-core.md` §2, §10 п. 8 | вердикт остаётся в конвенции «маркер + код» | M3 (host bridge и guest executor) — нет |
| Хеши ядра и initramfs, напечатанные гостем, сверяются с хешами файлов на хосте | сценарий `normal` (`{kernel_sha256}`, `{initrd_sha256}`) | гость загрузил именно собранные артефакты | M0 — нет: ядро печатает BootInfo, но хеши ядра в serial с хостом не сверяются (раздел 2.1, `normal`) |
| Переключатели fault injection есть в сборке, но включаются только режимом `nanox.test=` | `kernel/main.c`, `kernel/m2test.c`, `kernel/m3test.c`, `kernel/m4test.c`, `kernel/m5test.c` | один и тот же образ для положительного сценария и его контроля | M0 — есть в Rust M0: режим сбоя задаётся `boot_epoch` в BOOT.CFG образа только в test-профиле, бинарники EFI/ELF те же |
| Секрет не должен появляться в serial-логе — отдельная host-проверка | `check_key_secret` в `tools/bridge/m5scripts.py` | утечка ключа в лог, который сохраняется в записи запуска, обнаруживается автоматически, а не при чтении | M5 («API credentials хранятся и используются без вывода в обычные логи») — нет |

---

## 2. Сценарии QEMU по этапам ARCHITECTURE.md §15

Всего в `tests/qemu/scenarios.json` **54** сценария: M0 — 8, M1 — 13, M2 — 5,
M3 — 7, M4 — 9, M5 — 12. Из них **10** явно помечены в описании как
«Negative control»; `timer-masked` по смыслу тоже отрицательный контроль
(самопроверки таймера), но помечен как fault-injection.

Обозначения типа: **П** — позитивный; **П+И** — позитивный с внедрённым
сбоем (ожидается PASS, т. е. восстановление); **И** — fault-injection с
ожидаемым отказом (проверяется путь ошибки); **ОК** — отрицательный контроль
(та же проверка с намеренно сломанным механизмом обязана провалиться).

Переносимость: **высокая** — чёрный ящик (serial + код + host), от языка не
зависит; **средняя** — нужен эквивалентный крючок в ядре/госте или
изменится формат; **низкая** — привязано к деталям C-сборки или x86-ABI
C-кода.

### 2.1 M0 — стенд (8)

Описание: `docs/m0-bench.md` §6. Режимы: `kernel/main.c`.

| Сценарий | Что проверяет | Тип | Вердикт, код | Файлы | Переносимость | Rust-этап |
| --- | --- | --- | --- | --- | --- | --- |
| `normal` | полная загрузка до `TEST PASS`; хеши ядра и initramfs из лога = хешам на хосте; RSDP; с M1 — все маркеры шагов ядра, `ram_mapped` 250–262 MiB | П | PASS, 33 | `tests/qemu/scenarios.json`, `docs/m1-kernel.md` §2 | высокая (набор маркеров — свой) | M0 — частично есть: Rust-сценарий pass проверяет цепочку маркеров, BootInfo и статус 33; сверки напечатанных гостем хешей ядра с хостом нет, initramfs в Rust M0 нет, маркеры шагов ядра — M1 |
| `fail` | ядро само сообщает `TEST FAIL` | И | `test_fail`, 35 | `kernel/main.c` | высокая | M0 — есть в Rust M0 (kernel-fail: FAIL, 35) |
| `unknown-test` | неизвестный `nanox.test=bogus` — провал, не PASS | И | `test_fail`, 35 | `kernel/main.c` | высокая | M0 — нет: режим теста задаётся `boot_epoch`, неизвестное значение в test-профиле даёт PASS ([`b8913fa:kernel/src/main.rs` L190–204](https://github.com/ANngrock/NANOX-OS/blob/b8913fa5a89a288347deae389f53d7f2b4a9261b/kernel/src/main.rs#L190-L204)) |
| `panic` | путь panic, backtrace содержит `kernel_main` | И | `panic`, 37 | `kernel/panic.c` | средняя (backtrace и символы в Rust) | M0 — частично есть: kernel-panic (PANIC, но код 35, не отдельный); backtrace нет |
| `hang` | вечный останов; вердикт только по таймауту harness (20 с); в логе `kernel_main` и `test hang` | И | `timeout`, — | `tools/bench/harness.py` | высокая | M0 — есть в Rust M0 (kernel-hang: TIMEOUT, маркер `NANOX:TEST:HANG`) |
| `missing-kernel` | нет `KERNEL.ELF` на ESP | И | `loader_error` `E_KERNEL_OPEN`, 39 | `tools/image/mkimage.py` (`--omit-kernel`), `docs/boot-info.md` §8 | высокая | M0 — есть в Rust M0 (missing-kernel: LOADER_ERROR, 35, ядро не вошло) |
| `corrupt-kernel` | один байт ядра инвертирован после записи манифеста | И | `loader_error` `E_KERNEL_HASH`, 39 | `tools/image/mkimage.py` (`--corrupt-kernel`) | высокая | M0 — частично есть: truncated-elf и bad-segments отвергают структурно неверный ELF; манифеста хешей ядра нет, инвертированный байт в корректном ELF не обнаруживается (предварительно — подтвердить Codex) |
| `framebuffer` | каноника + ровно одно `-device VGA`; необязательные поля framebuffer в boot info | П | PASS, 33 | `tools/bench/qemu.py`, `docs/boot-info.md` §9 | высокая | M7 («Framebuffer/input/compositor») — нет (предварительно — подтвердить Codex) |

### 2.2 M1 — собственная загрузка и ядро (13)

Описание: `docs/m1-kernel.md` «Сценарии стенда», §4. Режимы:
`kernel/faults.c`. Для исключений harness сверяет мнемонику, вектор, код
ошибки, CR2, функцию RIP по `.symtab` (`tools/bench/elfsym.py`) и строку обхода
таблиц страниц.

| Сценарий | Что проверяет | Тип | Вердикт, код | Файлы | Переносимость | Rust-этап |
| --- | --- | --- | --- | --- | --- | --- |
| `missing-initrd` | нет `INITRD.IMG` | И | `loader_error` `E_INITRD_OPEN`, 39 | `tools/image/mkimage.py` | высокая | M2 (initramfs как источник userspace ELF) — нет (предварительно — подтвердить Codex) |
| `corrupt-initrd` | байт initramfs инвертирован | И | `loader_error` `E_INITRD_HASH`, 39 | `tools/image/mkimage.py` | высокая | M2 — нет (предварительно — подтвердить Codex) |
| `bad-initrd` | 150 байт текста вместо cpio, манифест им **соответствует** — отвергает уже ядро | И | `panic` `initramfs invalid: E_MAGIC at offset 0`, 37 | `kernel/initramfs.c` | высокая (если формат initramfs — cpio newc) | M2 — нет (предварительно — подтвердить Codex) |
| `ud` | `ud2` → `#UD`, вектор 6, RIP в `nx_fault_ud` | И | `exception`, 41 | `kernel/faults.c` | средняя (имя функции: Rust-манглинг/инлайнинг) | M1 («GDT/IDT, исключения…») — нет |
| `gp` | загрузка по неканоническому адресу → `#GP`, 13 | И | `exception`, 41 | `kernel/faults.c` | средняя | M1 — нет |
| `divzero` | `#DE`, 0 | И | `exception`, 41 | `kernel/faults.c` | средняя (в Rust деление на 0 — паника до `div`; нужен asm) | M1 — нет |
| `pagefault` | запись в `0xdead0000` → `#PF`, error `0x2`, CR2, `not-mapped` | И | `exception`, 41 | `kernel/faults.c`, `kernel/panic.c` | средняя | M1 («Проверены … page fault») — нет |
| `nullderef` | чтение по нулю → `#PF`, error `0x0`, CR2 `0x0` | И | `exception`, 41 | `kernel/faults.c` | средняя (в Rust нужен `read_volatile` по сырому указателю) | M1 — нет |
| `wprotect` | запись в `.rodata` при `CR0.WP` → `#PF`, error `0x3`, права `r--` | И | `exception`, 41 | `kernel/faults.c`, `kernel/mm/vmm.c` | средняя | M1 («проверка page permissions») — нет |
| `nxexec` | переход в данные при `EFER.NXE` → `#PF`, error `0x11`, CR2 = RIP | И | `exception`, 41 | `kernel/faults.c` | средняя | M1 («проверка page permissions») — нет |
| `stackoverflow` | рекурсия в страницу-ограничитель → `#DF` (8) на IST; строка `guard page of stack boot hit` | И | `exception`, 41 | `kernel/arch/x86_64/gdt.c`, `kernel/arch/x86_64/kernel.ld` | средняя; см. камень 4.5 (RIP при `#DF`) | M1 («guard pages», «аварийный стек») — нет |
| `doublefree` | повторное освобождение страницы → allocator отказывает `E_DOUBLE_FREE`, panic; backtrace `nx_page_free`, `kernel_main` | И | `panic`, 37 | `kernel/mm/pmm.c` | средняя (имена функций) | M1 («double free») — нет |
| `timer-masked` | LVT таймера оставлен замаскированным; самопроверка обязана заметить `ticks=0 expected=30` | И (по смыслу ОК самопроверки) | `test_fail`, 35 | `kernel/arch/x86_64/timer.c` | высокая | M1 («timer … работают») — нет |

Повторяемость: `normal` и `pagefault` ×3 (раздел 1.6).

### 2.3 M2 — изоляция, потоки, IPC (5)

Описание: `docs/m2-kernel.md` §10. Режимы: `kernel/m2test.c`. Программы:
`user/test/`.

| Сценарий | Что проверяет | Тип | Вердикт, код | Файлы | Переносимость | Rust-этап |
| --- | --- | --- | --- | --- | --- | --- |
| `m2-user` | `bin/hello` в ring 3 (`cpl=3`), свой корень таблиц, код `user,r-x`, стек `user,rw-`, в таблицах ядра этих адресов нет; `.data` из файла, `.bss` обнулён; после reap `free_pages`, `tables`, `tasks` равны исходным (обратные ссылки в регулярном выражении) | П | PASS, 33 | `kernel/task.c`, `kernel/mm/vmm.c`, `lib/elf_plan.c` | средняя; фиксированный адрес `data=0x8000002000` в шаблоне — хрупкий (камень 4.9) | M2 («Userspace ELF … в ring 3 с отдельным адресным пространством») — нет |
| `m2-sched` | три `bin/spin` без `yield` чередуются по таймеру; результат каждой = независимо посчитанному ядром; ядро и harness (`interleave`, `groups: 3`) независимо проверяют: первая строка прогресса каждой задачи раньше последней строки любой | П | PASS, 33 | `kernel/task.c`, `tools/bench/harness.py` | высокая (правило чередования — язык-нейтрально) | M1 («CPU-bound поток вытесняется», kernel threads); вариант с пользовательскими задачами — M2 (предварительно — подтвердить Codex) — нет |
| **`m2-sched-nopreempt`** | те же задачи без вытеснения идут подряд; проверка чередования обязана сработать (`preempted=0,0,0`, `tasks did not interleave`) | **ОК** к `m2-sched` | `test_fail`, 35 | `kernel/m2test.c` | высокая | M1/M2, вместе с `m2-sched` (предварительно — подтвердить Codex) — нет |
| `m2-ipc` | сообщение + handle объекта памяти с урезанными правами `READ\|MAP` (`0x5`); лишнее право → `EACCESS`; после передачи handle у отправителя недействителен; получатель отображает только на чтение, запись и дублирование → `EACCESS`; объекты уничтожены | П | PASS, 33 | `kernel/obj/handle.c`, `kernel/obj/ipc.c` | средняя (конкретные права — свои) | M2 («Handles, subset duplicate…», IPC) — нет |
| **`m2-ipc-overgrant`** | ядро намеренно отдаёт все права отправителя (`queued=0x1f`); получатель обязан заметить (`rights=0x1f expected=0x5`, код 2) | **ОК** к `m2-ipc` | `test_fail`, 35 | `kernel/m2test.c` (`nx_inject_ipc_overgrant`) | высокая | M2 — нет |

Повторяемость: `m2-sched` ×3 с `repeat.unordered` (раздел 1.6).

### 2.4 M3 — NCI, task engine, host bridge (7)

Описание: `docs/m3-core.md` §8. Все сценарии: режим ядра `m3-serve` (или
режим контроля), host-скрипт `"bridge": {"script": …}` из
`tools/bridge/scripts.py`, канал COM2 ↔ unix-сокет (`tools/bridge/session.py`).
Коды выхода `bin/core`: 0 — PASS; 3 — провал проверки конечного состояния;
4 — провалены host-проверки; 5 — host молчал 120 с.

| Сценарий | Что проверяет | Тип | Вердикт, код | Файлы | Переносимость | Rust-этап |
| --- | --- | --- | --- | --- | --- | --- |
| `m3-agent` | мок-модель: `list_tasks` → `spawn_task(load)` → `measure_task` → `terminate_task` с `expect_rev` → `list_tasks`; каждое действие `SUCCEEDED verify=ok`; host отдельно видит `GONE`; трасса полна; ресурсы вернулись | П | PASS, 33; `bridge.ok` | `tools/bridge/agent.py`, `tools/bridge/adapters.py`, `user/core/core.c` | высокая (протокол); NCI-формат — свой | M3 («ИИ получает список задач, запускает нагрузку, измеряет и останавливает её»; Verifier) — нет; в Rust M3 требуется настоящая модель, в C была мок-модель |
| `m3-nci` | все операции NCI v1; события задачи ровно `created, started, killed, reaped`, номера подряд, повторный poll пуст; некорректные запросы: `REJECTED UNKNOWN_OP`, `REJECTED duplicate_argument`, `FAILED BAD_REQUEST effects=none` | П | PASS, 33 | `user/core/nci.c`, `kernel/obj/event.c`, `tools/bridge/nci.py` | высокая | M3 (framed COM2 protocol; события с sequence) — нет |
| **`m3-events-off`** | ядро не пишет события; проверка исполнителя `no_created_event` и host-проверка обязаны упасть | **ОК** к `m3-nci` | `test_fail`, 35; проблема `^spawn: task.spawn: FAILED VERIFY_FAILED no_created_event$` | `kernel/m3test.c` | высокая | M3 («События имеют sequence, overflow marker и resync») — нет |
| `m3-faults` | ссылка из другой загрузки → `STALE_REF`; невыданный id → `NOT_FOUND`; `expect_rev=999` → `CONFLICT`; повтор запроса → тот же ответ `replayed=1`; тот же id с другим запросом → `ID_REUSED`; потерянный ответ → `action.status known=yes`; неотправленный запрос → `known=no`, затем исполняется один раз; старая ссылка → `GONE`, новая задача не задета | П+И | PASS, 33 | `user/core/engine.c`, `tools/bridge/scripts.py` | высокая | M3 («stale object, duplicate request, потерянный ответ и отмена») — нет |
| **`m3-nodedup`** | исполнитель не дедуплицирует: повтор запускает третью задачу; host-проверки `retry_not_replayed`, `retry_created_second_task` обязаны сработать | **ОК** к `m3-faults` | `test_fail`, 35 | `kernel/m3test.c` | высокая | M3 — нет |
| `m3-model-down` | адаптер `unavailable`: 3 попытки, задача host `FAILED model_unavailable`, ни одного действия у гостя; гость управляем без модели | П+И | PASS, 33 | `tools/bridge/adapters.py`, `tools/bridge/agent.py` | высокая | M3 («Сбой provider сохраняет recovery shell») — нет |
| **`m3-kill-noop`** | `TASK_KILL` возвращает успех без эффекта; исполнитель обязан ответить `VERIFY_FAILED still_running`, host-задача `FAILED action_failed` | **ОК** к `m3-agent` (критерий 4) | `test_fail`, 35; `core#4 exited with code 3` | `kernel/m3test.c` (`nx_inject_kill_noop`) | высокая | M3 («Verifier проверяет состояние задачи после операции») — нет |

### 2.5 M4 — постоянное состояние (9)

Описание: `docs/m4-store.md` §9–§11. Режимы: `kernel/m4test.c`; хранилище:
`lib/store.c`; независимая host-реализация формата: `tools/store/nxstore.py`;
host-скрипты: `tools/bridge/m4scripts.py`; вердикт серии сбоев:
`tools/bench/storecheck.py`.

| Сценарий | Что проверяет | Тип | Вердикт, код | Файлы | Переносимость | Rust-этап |
| --- | --- | --- | --- | --- | --- | --- |
| `m4-blk` | оба virtio-blk по PCI; загрузочный `ro=yes` отвергает запись; 8 блоков записаны, сброшены, прочитаны, восстановлены; эмулируемый кэш держит запись до flush; диск после — пустое хранилище | П | PASS, 33; `store.gen=1` | `kernel/dev/blk.c`, `kernel/dev/virtio.c` | высокая (шаблон `id=0x10(01\|42)` учитывает legacy/modern id) | M4 (virtio-blk; в Rust — userspace-драйвер) — нет |
| `m4-crash` | серия сбоев: остановка перед каждой из 60 операций записи/flush встроенной нагрузки × политики `all`/`none`/`torn`/`reorder` (те, что меняют исход), затем загрузка проверки; восстановлено поколение между последним `saved` и последним `begin`, содержимое = модели, host-читатель и обе проверки целостности согласны | И (серия) | `violations: 0`, `points_min: 60`; загрузки сбоя — `crash_point`, 43 | `tools/bench/harness.py` (crash-sweep), `tools/bench/storecheck.py`, `kernel/m4test.c` | средняя: методика — высокая; нужен слой сбоев в ядре и нумерация I/O | M4 («torn writes, cutpoints») — нет |
| `m4-persist` | 2 загрузки на одном диске: конфигурация, blob, закрепление, `task.spawn/terminate` с write-ahead; после перезагрузки — те же значения, записи task engine, повтор запроса 1-й загрузки из записи, `ID_REUSED`, журнал фиксаций, следующая фиксация `gen+1` | П | PASS/PASS | `user/core/persist.c`, `tools/bridge/m4scripts.py` | высокая | M4 («После reboot задачи восстанавливаются…») — нет |
| **`m4-persist-amnesia`** | 2-я загрузка получает чистый диск (`data_disk: "fresh"`); host-проверки `persist_status`, `persist_config`, `persist_task`, `persist_replay` обязаны упасть | **ОК** к `m4-persist` | PASS / `test_fail`, 35 | `tests/qemu/scenarios.json` | высокая | M4 — нет |
| `m4-corrupt-root` | между загрузками испорчен байт корня поколения 3; откат к 2 (`bad_root`), фиксация 4 в испорченный слот | П+И | PASS/PASS | `tools/store/nxstore.py` (`corrupt`) | высокая | M4 («corrupt metadata») — нет |
| `m4-unmountable` | испорчены оба суперблока; `no_valid_root`, `bad_crc,bad_crc`; операции хранилища `NO_STORE effects=none`, прочее NCI работает; диск не изменён (`data_disk_unchanged`) | П+И | PASS/PASS | `tools/store/nxstore.py` | высокая | M4 («corrupt metadata») — нет |
| `m4-full` | blob по 32 KiB до `NO_SPACE`; отказ без эффекта и без записи; удаления и prune (с резервом) освобождают место; после перезагрузки то же поколение | П+И | PASS/PASS | `lib/store.c`, `tools/bridge/m4scripts.py` | высокая | M4 («full disk», GC) — нет |
| `m4-retention` | окно 4 поколения и закрепления: закреплённое вне окна читается, незакреплённое `PRUNED`, журнал помечает `pruned` | П | PASS, 33 | `lib/store.c` | высокая | M4 (GC, snapshot/restore) (предварительно — подтвердить Codex) — нет |
| **`m4-crash-noflush`** | `nanox.m4.flush=noop`: flush сообщает успех без записи; остановка сразу после каждого `store saved` (`points: "after-ack"`) с политикой `all` обязана потерять объявленное сохранённым | **ОК** к `m4-crash` | `violations_min: 1`, `^saved_lost:` (получено во всех 11 точках) | `kernel/m4test.c`, `tools/bench/storecheck.py` | высокая (при наличии слоя сбоев) | M4 — нет |

Детали серии (`docs/m4-store.md` §10): **153 пары загрузок сбоя** (сбой +
проверка): `all` 60, `none` 38, `torn` 38, `reorder` 17 — политики по числу
незафиксированных записей (`policies_for` в `tools/bench/storecheck.py`: 0 →
только `all`; 1 → `all`, `none`, `torn`; ≥ 2 → ещё `reorder`); плюс **1
эталонная точка** (полный прогон + загрузка проверки, `K = 61`) — итого **154
результата**. Эталонная точка добавляется в результаты отдельно и считается
под политикой `none` (`ref_point` и `results.append(ref_point)` в
`tools/bench/harness.py`), поэтому в `summary.by_policy` и в итогах
`docs/m4-store.md` §14 («154 пары загрузок (`all` 60, `none` 39, …)») стоит
`none` 39 — это 38 пар сбоя + эталон. Пары выполняются параллельно
(`NANOX_JOBS`, по умолчанию 3); точка, упавшая до контроллера M4, — ошибка стенда, перезапуск один раз,
записывается в `summary.retried` (камень 4.1). Детерминизм: операции `1…K−1`
загрузки сбоя совпадают с эталоном построчно.

### 2.6 M5 — сеть и автономный provider (12)

Описание: `docs/m5-net.md` §10. Режимы: `kernel/m5test.c`; сервисы стенда:
`tools/bench/m5host.py` (DNS, echo, TLS echo, QMP, провижининг диска);
тестовый provider: `tools/bench/provider.py` (**сценарная политика, не
языковая модель**); host-скрипты: `tools/bridge/m5scripts.py`; PKI:
`tools/net/pki.py`.

| Сценарий | Что проверяет | Тип | Вердикт, код | Файлы | Переносимость | Rust-этап |
| --- | --- | --- | --- | --- | --- | --- |
| `m5-net` | virtio-net, ARP, ICMP, DNS (в т. ч. NXDOMAIN), TCP, отказ в соединении, диагностика; link down/up через QMP `set_link` | П | PASS, 33 | `kernel/dev/virtio.c`, `lib/net/net.c`, `lib/net/tcp.c`, `user/core/m5net.c` | высокая | M5 (virtio-net, Ethernet/IP/UDP/TCP/DNS) — нет |
| `m5-net-loss` | то же при `nanox.m5.loss=rx:5,tx:7` (детерминированная потеря в ядре); ARP, DNS, TCP восстанавливаются | П+И | PASS, 33 | `kernel/m5test.c` | высокая (нужен детерминированный слой потерь) | M5 («Проверены потери, reorder…») — нет |
| `m5-tls` | CSPRNG из virtio-rng; TLS 1.3 против OpenSSL: AES-GCM и ChaCha20, ECDSA и RSA-PSS, промежуточный сертификат; отказы `expired`, `wrong-name`, `untrusted` — класс `tls`, OpenSSL видит alert, прикладные данные не ушли | П+И | PASS, 33 | `lib/tls/tls13.c`, `lib/tls/x509.c`, `user/core/m5tls.c`, `tools/net/pki.py` | высокая; см. камень 4.8 (гонка) | M5 (entropy/CSPRNG, TLS с trust store и политикой времени) — нет |
| `m5-agent` | Core сам спрашивает provider через свой TCP/TLS без bridge; вызовы инструментов — проверенные действия task engine; телеметрия; ключ не попадает в лог | П | PASS, 33 | `user/core/agent.c`, `user/core/provider.c`, `lib/http/messages.c` | высокая | M5 («Core вызывает provider непосредственно…», ключи не в логах) — нет |
| `m5-agent-loss` | `m5-agent` при потере rx:5, tx:7; TCP-повторы держат путь | П+И | PASS, 33 | `lib/net/tcp.c` | высокая | M5 — нет |
| **`m5-noretx`** | та же потеря, повторная передача TCP выключена; путь provider обязан упасть | **ОК** к `m5-agent-loss` | `test_fail`, 35; `^ask_workload: FAILED .*class=net code=NET_ERROR ` | `kernel/m5test.c` | высокая | M5 — нет |
| `m5-faults` | 429 с `Retry-After`, 529, 500, `error` в потоке, FIN без `close_notify`, ранний `close_notify`, молчание (тайм-аут), RST, закрытие keep-alive, исчерпание попыток, неповторяемые ошибки; каждая попытка классифицирована в телеметрии | П+И | PASS, 33 | `user/core/provider.c`, `tools/bench/provider.py` | высокая | M5 («Обрывы streaming, timeout и reconnect…») — нет |
| **`m5-noretry`** | клиент делает одну попытку; проверки повторов обязаны упасть | **ОК** к `m5-faults` | `test_fail`, 35; `^retry_5xx: FAILED .*detail=overloaded ` | `kernel/m5test.c` | высокая | M5 — нет |
| `m5-classes` | у одного гостя различаются `NET_ERROR` (refused, link down), `PROVIDER_ERROR` (HTTP 400, несуществующий инструмент), `ACTION_ERROR` — в ответе, телеметрии и `telemetry.status` | П+И | PASS, 33 | `lib/net/nerr.c`, `docs/m5-net.md` §9 | высокая | M5 (предварительно — подтвердить Codex) — нет |
| **`m5-flattel`** | классификация выключена (`unclassified`); проверки классов обязаны упасть | **ОК** к `m5-classes` | `test_fail`, 35; `^net_refused: FAILED .*class=unclassified code=UNCLASSIFIED_ERROR detail=conn_refused ` | `kernel/m5test.c` | высокая | M5 (предварительно — подтвердить Codex) — нет |
| `m5-console` | provider отказывает и линк опущен: агент падает чисто (`class=net`), консоль COM2 наблюдает, запускает/останавливает задачи, диагностирует; после возврата provider агент снова работает | П+И | PASS, 33 | `tools/bridge/m5scripts.py` | высокая | M5 (с M3: «recovery shell») (предварительно — подтвердить Codex) — нет |
| `m5-nokey` | нет объекта ключа: `LOCAL_ERROR no_key` до всякого соединения; консоль работает | П+И | PASS, 33 | `user/core/provider.c` | высокая | M5 («API credentials…») (предварительно — подтвердить Codex) — нет |

### 2.7 Сводка отрицательных контролей

| Контроль | К чему | Сломанный механизм | Ожидаемая причина провала | Rust-этап |
| --- | --- | --- | --- | --- |
| `m2-sched-nopreempt` | `m2-sched` | вытеснение | `tasks did not interleave` | M1/M2 (предварительно — подтвердить Codex) — нет |
| `m2-ipc-overgrant` | `m2-ipc` | урезание прав handle при передаче | `rights=0x1f expected=0x5` | M2 — нет |
| `m3-events-off` | `m3-nci` | журнал событий ядра | `no_created_event` | M3 — нет |
| `m3-nodedup` | `m3-faults` | дедупликация запросов | `retry_not_replayed`, `retry_created_second_task` | M3 — нет |
| `m3-kill-noop` | `m3-agent` | эффект `TASK_KILL` | `VERIFY_FAILED still_running` / `action_failed` | M3 — нет |
| `m4-persist-amnesia` | `m4-persist` | сохранность диска между загрузками | `persist_status`, `persist_config`, `persist_task`, `persist_replay` | M4 — нет |
| `m4-crash-noflush` | `m4-crash` | flush | `saved_lost` | M4 — нет |
| `m5-noretx` | `m5-agent-loss` | повторная передача TCP | `class=net code=NET_ERROR` | M5 — нет |
| `m5-noretry` | `m5-faults` | повтор запроса к provider | `retry_5xx` | M5 — нет |
| `m5-flattel` | `m5-classes` | классификация ошибок | `class=unclassified` | M5 (предварительно — подтвердить Codex) — нет |

Принцип (`docs/m5-net.md` §12 п. 9): отрицательный контроль для **каждого
механизма, влияющего на вердикт**. Рекомендуется переносить контроль вместе с
позитивным сценарием, а не позже.

---

## 3. Host-тесты

Источник: `tests/host/`, цели `host-test` и `py-test` в `Makefile`. C-тесты
собираются одним бинарником (`tests/host/test_main.c`, макросы
`tests/host/test.h`) с UBSan в режиме trap (`HOST_SAN` в `Makefile`);
дополнительно однократно собирались gcc с `-fsanitize=address,undefined`.
Итог последнего прогона: 2356 C-проверок, 114 Python-тестов
(`docs/m5-net.md` §13).

### 3.1 Что покрыто

| Модуль | Тест | Что стоит перенести | Rust-этап |
| --- | --- | --- | --- |
| Валидатор boot info | `tests/host/test_bootinfo.c` | все коды `E_*` из `docs/boot-info.md` §5 по отдельности; `test_mutations`: 20 000 итераций детерминированного xorshift-искажения 1–4 бит в boot info, cmdline, карте памяти и RSDP — валидатор не падает (UBSan), код всегда в диапазоне, принятая структура имеет верный magic, хотя бы одно искажение отвергнуто | M0 — частично есть: `b8913fa:crates/boot-protocol/tests/boot_info.rs` проверяет версию, флаги, указатели, пересечения, stride, усечение; массового искажения (аналог 20 000 итераций) нет |
| Перевод карты памяти UEFI | `tests/host/test_mmap.c` | выравнивание, переполнение, пересечения, ёмкость | M0/M1 — частично есть: stride, усечение, пересечения и reserved-диапазоны в `b8913fa:crates/boot-protocol/tests/boot_info.rs`; ёмкость и выравнивание для allocator — M1 (предварительно — подтвердить Codex) |
| Чтение initramfs (cpio newc) | `tests/host/test_initramfs.c` | все коды `E_TRUNCATED` … `E_NOT_FOUND`; 20 000 случайных искажений; чтение программ из собранного `out/initrd.img` | M2 (источник userspace ELF) (предварительно — подтвердить Codex) — нет |
| План загрузки ELF | `tests/host/test_elf.c` | границы и порядок сегментов, окно адресов, точка входа, W+X | M0 — есть в Rust M0: `b8913fa:crates/boot-protocol/tests/elf_validation.rs` + golden fixtures `b8913fa:tests/fixtures/` (усечение, пересечение, W+X, переполнение, выравнивание, окно адресов) |
| SHA-256 | `tests/host/test_sha256.c` | известные векторы | M5 (криптографические операции) (предварительно — подтвердить Codex) — нет: в Rust M0 SHA-256 только на хосте (crate `sha2`), гостевой реализации нет |
| Физический allocator | `tests/host/test_pmm.c` | `E_ALIGN`, `E_UNMANAGED`, `E_DOUBLE_FREE`, страницы ниже 1 MiB | M1 — нет |
| Таблицы страниц | `tests/host/test_pt.c` | построение, права, `test_destroy_slots` (утечки при разборке, пропуск листьев) | M1 («mapping/unmapping и проверка page permissions») — нет |
| Таблица handle | `tests/host/test_handle.c` | права при дублировании/передаче, поколения, полная таблица, счётчики ссылок | M2 — нет |
| Очередь IPC | `tests/host/test_ipc.c` | пустая/полная, копирование, перенос по кругу, опустошение | M2 — нет |
| Журнал событий | `tests/host/test_event.c` | порядок, вытеснение из кольца, выключенный журнал | M3 («События имеют sequence, overflow marker и resync») — нет |
| Разбор NCI | `tests/host/test_nci.c` | все ошибки разбора, границы длины, отпечатки, ссылки | M3 (framed COM2 protocol с лимитами) — нет |
| Task engine | `tests/host/test_engine.c` | все допустимые и запрещённые переходы, дедупликация, `ID_REUSED`, вытеснение, режим без дедупликации, восстановление | M3 — нет |
| Хранилище | `tests/host/test_store.c` | формат, объекты, версии, `test_retention`, `test_full`, `test_recovery`, ошибки устройства, `test_crash_simulation` (ниже) | M4 — нет |
| Сеть | `tests/host/test_net.c` | два стека на имитированной линии: потеря, дубли, переупорядочивание, нулевое окно; DNS; классы ошибок | M5 — нет |
| Криптография | `tests/host/test_crypto.c`, `tests/host/crypto_vectors.h`, `tests/host/gen_crypto_vectors.py` | векторы, сгенерированные hashlib/hmac/OpenSSL, плюс отрицательные случаи с инвертированным битом | M5 — нет |
| JSON/HTTP/SSE/Messages | `tests/host/test_http.c` | разбор и запись JSON, ответы HTTP, SSE, поток и тело Messages | M5 (предварительно — подтвердить Codex) — нет |
| TLS против OpenSSL | `tests/host/test_tls.py`, `tests/host/tlstool.c` | 14 тестов: оба набора, RSA-цепочка, wildcard, нет промежуточного, просрочен/ещё не действует, чужое имя, недоверенный УЦ, усечение; OpenSSL судит те же профили так же | M5 — нет |
| Harness | `tests/host/test_harness.py` | правила вердикта, разбор отчёта, маскирование, чередование, валидность сценариев | M0 — частично есть: тесты вердикта и таймаута в `b8913fa:tools/xtask/src/runner.rs` и `b8913fa:tools/xtask/src/record.rs`; маскирования, чередования и проверки файла сценариев нет |
| Образ и initramfs | `tests/host/test_mkimage.py`, `tests/host/test_mkinitrd.py` | детерминизм, GPT, геометрия FAT32, манифест, **обратное чтение через mtools**, fault injection образа | M0 — частично есть: тест искажения ядра в `b8913fa:tools/xtask/src/image.rs`; побайтная повторяемость образа и обратное чтение не автоматизированы (раздел 1.7); initramfs нет |
| Две реализации формата хранилища | `tests/host/test_nxstore.py`, `tests/host/storetool.c` | C читает Python-формат и наоборот, одинаковые решения восстановления | M4 (предварительно — подтвердить Codex) — нет |
| Вердикт серии сбоев | `tests/host/test_storecheck.py` | модель поколений, `saved_lost`, `content_mismatch`, ошибка стенда ≠ нарушение | M4 — нет |
| Bridge | `tests/host/test_bridge.py` | шум прошивки до `HELLO`, потеря ответа с дедупликацией и без, полнота трассы | M3 — нет |
| Сервисы и provider стенда | `tests/host/test_m5host.py`, `tests/host/test_provider.py` | DNS, провижининг диска, форма потока, сценарии сбоев | M5 (предварительно — подтвердить Codex) — нет |

Переносимость: **высокая** для самих случаев (таблицы входов/ожиданий),
**средняя** для кода (в Rust — `#[test]`/proptest вместо макросов
`tests/host/test.h`; UBSan заменяется Miri/отладочными проверками —
эквивалентность здесь не проверялась). Python-тесты стенда переносятся без
изменений, если стенд останется на Python; если стенд на `xtask`, переносятся
таблицы случаев.

### 3.2 Crash-симуляция хранилища на хосте

Источник: `crash_sim` и `test_crash_simulation` в `tests/host/test_store.c`,
`docs/m4-store.md` §10. Устройство с энергозависимым кэшем; нагрузка из 11
транзакций на диске из 64 блоков останавливается в каждой из 59 точек; для
каждой перебираются исходы незафиксированных записей (при ≤ 6 блоках — все
подмножества, иначе выборка + «оборванная половина» каждой записи) — 445
монтирований, 0 нарушений. Пять отрицательных контролей через флаги
`ST_TEST_*` (`lib/include/nanox/store.h`), каждый обязан дать нарушение и
даёт: «сохранено» до flush суперблока; flush отсутствует; нет барьера и нет
проверки при монтировании; один слот суперблока; занимаются блоки
сохраняемых поколений. Наблюдение: без **одного** барьера или без **одной**
проверки нарушений нет (каждый механизм достаточен в этой модели) — это
записано как результат, а не повод убрать механизм.

Переносимость: **высокая** (чистый код, не зависит от QEMU); в QEMU перебор
подмножеств заменён четырьмя политиками (раздел 2.5).

### 3.3 Mutation-check (ручное внесение ошибок)

В проверяемый host-тестами код по одной вносились ошибки и проверялось, что
тесты их замечают:

| Этап | Что вносилось | Результат | Источник |
| --- | --- | --- | --- |
| M1 | ошибки в allocator, построитель таблиц, чтение initramfs, валидатор | каждую обнаружили | `docs/m1-kernel.md` «Что проверено» |
| M2 | 17 ошибок: права при дублировании и передаче, поколения handle, граница таблицы, счётчик ссылок, переполнение и перенос очереди, утечки при разборке таблиц, лишний слот, пропуск листьев, W+X, зарезервированный бит | каждую обнаружили | `docs/m2-kernel.md` §11 |
| M3 | 8 ошибок: переход `RUNNING → SUCCEEDED`, пропуск сверки отпечатка, вытеснение новой записи вместо старой, запись событий при выключенном журнале, повтор ключа, ссылка с id 0, сдвиг границы кольца, удаление разделителя в отпечатке | 6 обнаружены; 2 последние эквивалентны (поведение не меняется), что проверено разбором кода | `docs/m3-core.md` §11 |
| M4 | 5 поломок протокола через `ST_TEST_*` | каждую обнаружили | `docs/m4-store.md` §10 |
| M5 | отдельной ручной мутации не описано; вместо неё — отрицательные векторы криптографии и отрицательные контроли QEMU | — | `docs/m5-net.md` §13, коммит `8ca07bf` |

Что это дало: подтверждение чувствительности тестов и два случая
эквивалентных мутантов, которые не стоит считать пробелом. Процедура была
ручной и не автоматизирована. **Предложение:** в Rust — `cargo-mutants` или
аналог с тем же правилом «эквивалентный мутант обосновывается разбором
кода»; применимость инструмента здесь не проверялась.

---

## 4. Подводные камни, найденные на практике

### 4.1 Самопроверка таймера нестабильна под TCG при параллельных гостях

- **Симптом.** Серия `m4-crash` дважды нашла «нарушения», которые оказались
  провалом самопроверки таймера M1 до запуска хранилища; позже — одна точка
  `K = 43`, `all` упала до контроллера M4.
- **Причина.** Самопроверка ждёт 20–40 прерываний за окно 30 × 10 мс по PIT;
  под нагрузкой хоста при параллельных гостях TCG это окно может не
  выполниться. В M1 наблюдалось 29–30 прерываний в 32 запусках, в том числе при
  4 параллельных QEMU (`docs/m1-kernel.md` §7) — то есть сбой редкий.
- **Решение в C.** Вердикт точки различает ошибку стенда (загрузка не дошла до
  контроллера M4) и нарушение; такая точка перезапускается **один раз**,
  перезапуск записывается (`summary.retried`) — `run_point` в
  `tools/bench/harness.py`, `test_infrastructure_failure_is_an_error` в
  `tests/host/test_storecheck.py`, `docs/m4-store.md` §10 п. 4, §13 п. 13.
- **Рекомендация.** Сразу разделить «ошибку стенда» и «нарушение» в модели
  вердикта; перезапуск — не больше одного и всегда в записи; не расширять
  допуск таймера молча. Под QEMU 9.2 поведение не проверялось.

### 4.2 Маскирование r8–r11 в повторе `pagefault`

- **Симптом.** Проверка повторяемости `pagefault` расходилась в `r9`, в том
  числе на ревизии до M4.
- **Причина.** Caller-saved регистр `r9` хранил остаток прежнего вызова —
  число тиков самопроверки таймера (29 или 30 в зависимости от хоста).
- **Решение в C.** Маскировать `r8`–`r11` **только** в строках `NANOX: REGS`
  (`REGS_SCRATCH_RE`, `tools/bench/harness.py`; тест
  `test_scratch_registers_masked_in_reports_only`); коммит `1de7b80`,
  `docs/m4-store.md` §14.
- **Рекомендация.** В Rust набор «грязных» регистров будет другим (другой код
  генерации). Не копировать маску вслепую: сначала получить расхождение, затем
  маскировать ровно те поля, чьё происхождение объяснено.

### 4.3 `isa-debug-exit`, `-no-reboot` и `-no-shutdown`

- **Факт.** Статус процесса = `(значение << 1) | 1`, поэтому PASS — 33, а не 0.
  `-no-reboot` превращает сброс/тройную ошибку в завершение QEMU со статусом 0
  (класс `unexpected_exit`) вместо бесконечной перезагрузки. `-no-shutdown`
  **не используется**: с ним QEMU не завершается при выключении
  (`docs/m0-bench.md` §2).
- **Рекомендация.** Сохранить обе настройки; статус 0 никогда не считать
  успехом (`test_other_exit_codes`). Вне QEMU устройства нет: загрузчик до
  `ExitBootServices` возвращает `EFI_LOAD_ERROR`, после — `cli; hlt`
  (`docs/boot-info.md` §8).

### 4.4 Повтор `ExitBootServices` не проверен

- **Факт.** Ветка повтора при изменившемся map key реализована (до 4 попыток,
  затем `E_EXIT_BOOT_SERVICES`), но на стенде не возникала
  (`docs/m1-kernel.md` «Что не проверено», ARCHITECTURE.md §15 M1).
- **Рекомендация.** Не считать C-код образцом проверенного поведения; если
  нужна проверка — отдельный fault-injection (например, принудительно
  устаревший map key в тестовом режиме загрузчика). Такого сценария в C нет.

### 4.5 Сохранённый RIP при `#DF` не гарантирован

- **Факт.** По SDM сохранённые CS:RIP при `#DF` не определены; QEMU сообщает
  RIP команды исходной ошибки, и `stackoverflow` на это опирается
  (`rip_function: nx_fault_stack_overflow`) — `docs/m1-kernel.md` «Что не
  проверено».
- **Рекомендация.** В Rust-сценарии опираться на строку о странице-ограничителе
  (CR2 в guard page) и вектор 8, а RIP-проверку считать специфичной для QEMU.

### 4.6 OVMF пишет в COM2 до загрузки

- **Симптом/причина.** Прошивка выводит в COM2 свой текст до загрузки ядра;
  канал bridge видит этот шум.
- **Решение в C.** Host пропускает всё до строки `HELLO`
  (`docs/m3-core.md` §2; `test_hello_after_firmware_noise_and_call` в
  `tests/host/test_bridge.py`). В COM1 ANSI-последовательности OVMF удаляются
  перед разбором (раздел 1.1).
- **Рекомендация.** Любой служебный канал через UART — с явной точкой
  синхронизации.

### 4.7 slirp: потерянный SYN-ACK повторяется лишь через ~6 с

- **Симптом.** В `m5-net-loss` одна потеря превращалась в отказ соединения.
- **Причина.** slirp повторяет потерянный SYN-ACK примерно через 6 с, а на
  повторный SYN отвечает голым ACK.
- **Решение в C.** Тайм-аут соединения 15 с (`docs/m5-net.md` §4, §12 п. 2).
- **Рекомендация.** Тайм-ауты сетевого стека выбирать с учётом slirp; под
  QEMU 9.2 не проверялось.

### 4.8 Гонка в `m5-tls`

- **Симптом.** Однажды host-проверка «OpenSSL увидел alert» провалилась.
- **Причина.** Поток TLS-сервера записывает ошибку рукопожатия **после** того,
  как гость уже ответил; проверка читала список ошибок раньше.
- **Решение в C.** Ожидание с дедлайном 5 с перед проверкой
  (`tools/bridge/m5scripts.py`, цикл перед `tls_%s_alert`; коммит `f3fa10e`).
- **Рекомендация.** Во всех host-проверках, которые читают состояние другого
  потока/процесса, — ожидание условия с дедлайном, а не мгновенное чтение.

### 4.9 Хрупкие точные адреса в шаблонах

- **Симптом.** Во время M5 сдвинулся адрес данных `m2-user` из-за таблицы
  имён ошибок (`docs/m5-net.md` §13).
- **Причина.** Шаблон `m2-user` требует `data=0x8000002000` — точный адрес,
  зависящий от раскладки `bin/hello`.
- **Рекомендация.** Проверять свойство («в пользовательской половине»,
  «выровнено», «`data_ok=yes`»), а не конкретный адрес, если адрес не
  является частью контракта.

### 4.10 Прочие находки сценариев

Источник: `docs/m5-net.md` §13 (что нашли сценарии во время подготовки):
тупик нулевого окна и медленное восстановление TCP при потере (исправлено
быстрой повторной передачей и буфером вне порядка); указатели на уже
сдвинутый буфер рукопожатия в проверке CertificateVerify с RSA-цепочкой.
Рекомендация: сохранить `m5-net-loss` и RSA-цепочку в `m5-tls` — именно они
эти ошибки нашли.

### 4.11 Мелкие, но полезные

- **Хеши артефактов на хосте, а не в госте** — `normal` сравнивает
  напечатанный гостем SHA-256 с хешем файла (раздел 1.9).
- **Шаблоны с `{}`** — при подстановке `{kernel_sha256}` литеральные скобки
  удваиваются (`{{3}}` в `m2-user`); тест `test_pattern_substitution`.
- **Программная точка останова под KVM затирается** копированием ядра —
  поэтому `hbreak` (раздел 1.8).
- **Backtrace по RBP** требует `-fno-omit-frame-pointer`; функции после
  хвостового вызова в цепочке не видны (`docs/m1-kernel.md` §4). В Rust
  аналог — принудительные frame pointers; не проверялось.

---

## 5. Что в C не проверено (не использовать как готовый образец)

| Пункт | Состояние в C | Источник |
| --- | --- | --- |
| M2, критерий 4: некорректный syscall не повреждает ядро | реализовано, **сценариев нет**; `nx_inject_uaccess_unchecked` ни одним режимом не включается; host-тестов `kernel/mm/uaccess.c` нет | `docs/m2-kernel.md` §12 |
| M2, критерий 5: приложение не читает чужую память | реализовано, **сценариев нет**; путь `nx_user_trap` не выполнялся | `docs/m2-kernel.md` §12 |
| M2, критерий 6: привилегированная операция Sovereign | `SOV_TASK_OPEN`, `TASK_READ` не выполнялись; `TASK_KILL` — только через handle из `SOV_TASK_SPAWN` в M3 | `docs/m2-kernel.md` §12, `docs/m3-core.md` §12 |
| M3, критерий 3: ИИ по запросу выполняет цепочку | только **мок-модель**; готовность M3 не достигнута | `docs/m3-core.md` §1, §12 |
| Настоящий provider | адаптер `anthropic` (M3) и путь в госте (M5) написаны, **ни одного запроса** не выполнено: нет ключа и внешнего доступа | `docs/m3-core.md` §12, `docs/m5-net.md` §14 |
| Собственная криптография и TLS | векторы и совместимость с OpenSSL; **аудита нет**, постоянное время и стойкость к атакам не испытывались; HelloRetryRequest и отзыв не поддержаны | `docs/m5-net.md` §14 |
| TLS-совместимость | только против OpenSSL (Python `ssl`) с тестовой PKI | `docs/m5-net.md` §14 |
| Эмулятор | всё — QEMU 8.2.2, **только TCG**; KVM и реальное железо не проверялись | `docs/m0-bench.md` §9, ARCHITECTURE.md §15 |
| Nix | `flake.nix` написан, **не проверен**; `flake.lock` не создан; профиль `nix` помечен unverified | `docs/m0-bench.md` §3 |
| Durability | модель сбоя — остановка гостя + эмулируемый кэш ядра; настоящее отключение питания, кэш QEMU/хоста, ошибки ввода-вывода в QEMU не проверялись; оборванная запись в QEMU — одна форма | `docs/m4-store.md` §15 |
| Прочее M1 | NMI и `#MC` не доставлялись; один CPU; повтор `ExitBootServices` | `docs/m1-kernel.md` |
| Прочее M3 | переполнение журнала событий и вытеснение записей task engine — только host-тестами; обрыв канала и враждебные данные не моделировались; повторяемость M3 не настраивалась | `docs/m3-core.md` §12 |
| Сеть | только slirp, IPv4, без фрагментации и контроля перегрузки; потеря — детерминированный слой, не реальная сеть | `docs/m5-net.md` §14 |

Разница окружений: C проверялся на QEMU 8.2.2 с `pc-q35-8.2`, Rust-стенд —
QEMU 9.2.4 с `pc-q35-9.2`. Числовые допуски (таймер 20–40 тиков, тайм-ауты
slirp, `ram_mapped` 250–262 MiB, PCI-адреса) под 9.2 **не проверялись**.

---

## 6. Предлагаемый порядок переноса (рекомендация)

Решение за Codex и владельцем. Этапы ROADMAP M0–M10 для каждого пункта — в
пометках и колонке «Rust-этап» разделов 1–2 (заполнено Claude, требует
подтверждения Codex).

1. **Модель вердикта и её тесты** (раздел 1.2–1.3, таблицы случаев из
   `tests/host/test_harness.py`) — дёшево, язык-нейтрально, сразу защищает от
   ложного PASS.
2. **Run record** (1.5) и **repro-check с образами дисков и отрицательным
   контролем** (1.7).
3. **Сценарии отказов загрузчика и ядра** M0/M1 (`fail`, `unknown-test`,
   `panic`, `hang`, `missing-*`, `corrupt-*`, `bad-initrd`, исключения,
   `doublefree`, `timer-masked`) + `debug-check` (1.8) + `repeat` с
   объяснёнными масками (1.6).
4. **Host-тесты чистых модулей** по мере появления кода: валидатор boot info с
   20 000 искажений, cpio, allocator, таблицы страниц (3.1).
5. **M2-сценарии вместе с их контролями** (`m2-sched` + `m2-sched-nopreempt`,
   `m2-ipc` + `m2-ipc-overgrant`) и, в отличие от C, **сразу сценарии
   критериев M2 4–6** (раздел 5).
6. **M3/M4/M5** — сценарий и его отрицательный контроль одним шагом
   (таблица 2.7); серия сбоев M4 — после появления нумерованного слоя
   ввода-вывода; crash-симуляция хранилища на хосте — раньше QEMU-серии.
7. Мутационная проверка (3.3) — на каждом этапе, до объявления критерия
   выполненным.
