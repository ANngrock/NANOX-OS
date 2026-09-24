# M0: воспроизводимый стенд

Документ фиксирует решения этапа M0 (ARCHITECTURE.md §15): конфигурацию
QEMU/UEFI, версии toolchain, сборку образа, headless harness, формат записи
запусков, коды выхода, serial-маркеры, проверку воспроизводимости и порядок
отладки через GDB. Интерфейс boot info, структура памяти и правила ошибок —
в [boot-info.md](boot-info.md).

Конвенции ниже (маркеры, коды выхода, формат записи, побайтовая
воспроизводимость) в ARCHITECTURE.md не заданы буквально; это решения M0,
принятые для выполнения его критериев.

## 1. Одна последовательность от чистого checkout

```sh
make doctor && make && make test
```

| Цель | Что делает | Нужны QEMU/OVMF |
| --- | --- | --- |
| `make doctor` | сверяет инструменты с `toolchain.lock`, пишет `out/doctor.json` | да (проверяются версии и хеши) |
| `make` | собирает `out/BOOTX64.EFI`, `out/kernel.elf`, `out/initrd.img` (с M1), `out/nanox.img`, `out/SHA256SUMS` | нет |
| `make test` | `host-test` + `py-test` + `qemu-test` (все сценарии) | да |
| `make run` | сценарий `normal` с выводом serial в терминал и записью запуска | да |
| `make debug` | QEMU с `-s -S`, serial в терминал; см. раздел 8 | да |
| `make debug-check` | автоматическая проверка GDB-процедуры | да, плюс gdb |
| `make repro-check` | три сборки в разных путях, побайтовое сравнение | нет |
| `make clean` / `make distclean` | удалить артефакты / также записи запусков `out/runs` | нет |

Образ ни на одном шаге не редактируется вручную: сценарные образы harness
собирает сам из тех же `BOOTX64.EFI` и `kernel.elf`.

## 2. Конфигурация QEMU/UEFI

Единственный источник командной строки — `tools/bench/qemu.py`
(`python3 tools/bench/qemu.py print` печатает её). Канонический вид:

```text
qemu-system-x86_64 -no-user-config -nodefaults -machine pc-q35-8.2 -accel tcg
  -cpu qemu64 -smp 1 -m 256M -rtc base=2026-01-01T00:00:00,clock=vm
  -display none -monitor none -no-reboot
  -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd
  -drive if=pflash,format=raw,unit=1,file=<run>/OVMF_VARS.fd
  -drive if=none,id=nxdisk,format=raw,readonly=on,file=<image>
  -device virtio-blk-pci,drive=nxdisk,bootindex=0
  -device isa-debug-exit,iobase=0xf4,iosize=0x01
  -serial file:<run>/serial.log
```

| Опция | Причина |
| --- | --- |
| `-no-user-config -nodefaults` | не читать конфигурацию хоста; никаких устройств по умолчанию (VGA, NIC, ...) |
| `-machine pc-q35-8.2` | версионированный тип машины вместо алиаса `q35`: новые версии QEMU сохраняют его поведение |
| `-accel tcg` | KVM в эталонной среде недоступен; TCG одинаков на любом хосте |
| `-cpu qemu64 -smp 1 -m 256M` | фиксированные модель CPU, число CPU и объём памяти |
| `-rtc base=2026-01-01T00:00:00,clock=vm` | часы гостя не зависят от даты хоста |
| `-display none -monitor none` | headless |
| `-no-reboot` | сброс или тройная ошибка завершают QEMU (статус 0), а не зацикливают загрузку |
| pflash `OVMF_CODE_4M.fd` read-only | прошивка UEFI, закреплённая по SHA-256 |
| pflash `OVMF_VARS.fd` | свежая копия шаблона NVRAM на каждый запуск: одинаковое начальное состояние переменных |
| `virtio-blk-pci ... readonly=on, bootindex=0` | загрузочный диск — virtio-blk (ARCHITECTURE.md §14); образ не меняется запуском |
| `isa-debug-exit,iobase=0xf4,iosize=0x01` | гость сообщает вердикт кодом выхода процесса |
| `-serial file:` | единственный канал диагностики до GUI |

`-no-shutdown` не используется: иначе QEMU не завершается при выключении.

Прошивка: пакет Ubuntu `ovmf 2024.02-2ubuntu0.9`,
`OVMF_CODE_4M.fd` SHA-256
`949bfa5389c4c48582737481e7d24f46b3a16b276ef44c4089a56858c6a0a446`,
`OVMF_VARS_4M.fd` SHA-256
`5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e`.
Пути переопределяются переменными `NANOX_OVMF_CODE` / `NANOX_OVMF_VARS`,
бинарник QEMU — `NANOX_QEMU`; `make doctor` проверит хеши выбранных файлов.

Единственное отклонение от канонической конфигурации — сценарий
`framebuffer`, который добавляет ровно одно устройство `-device VGA`, чтобы
проверить необязательные поля framebuffer в boot info.

## 3. Toolchain

| Инструмент | Версия (профиль `ubuntu-24.04`) | Роль |
| --- | --- | --- |
| clang | 18.1.3 | загрузчик (`--target=x86_64-unknown-windows`), ядро (`--target=x86_64-unknown-none-elf`), host-тесты |
| lld-link | 18.1.3 | PE/COFF, `/subsystem:efi_application` |
| ld.lld | 18.1.3 | ELF64 ядра |
| QEMU | 8.2.2 (`1:8.2.2+ds-0ubuntu1.18`) | стенд |
| OVMF | 2024.02-2ubuntu0.9 (по хешам) | UEFI |
| mtools | 4.0.43 | независимая проверка FAT32 образа в тестах |
| python3 | ≥ 3.8 (проверено 3.11.15) | mkimage, harness, doctor; только стандартная библиотека |
| GNU make | ≥ 4.0 (проверено 4.3) | сборка |
| gdb | не закреплён (проверено 15.1) | только `make debug` / `make debug-check` |

`toolchain.lock` хранит профили. `tools/doctor.py` выбирает профиль
(`--profile`, `NANOX_TOOLCHAIN_PROFILE`, `nix` при `IN_NIX_SHELL`, иначе
`ubuntu-24.04`), сравнивает версии из `--version` и хеши прошивки и
завершается с ошибкой при любом расхождении. Профиль без `status = verified`
отвергается. `tools/doctor.py --print-lock` печатает секцию для текущей
машины; изменение закреплённых значений — отдельный осознанный коммит.

`flake.nix` описывает то же окружение из nixpkgs, закреплённого коммитом
`50ab793786d9de88ee30ec4e4c24fb4236fc2674` (ветка `nixos-24.11`): clang/lld
18.1.8, QEMU 9.1.3, OVMF, mtools 4.0.45, python3, make, gdb. **Ни flake, ни
профиль `nix` ещё не проверялись**: Nix в среде подготовки M0 отсутствовал,
`flake.lock` не создан, хеши OVMF для Nix неизвестны. Порядок проверки:

```sh
nix flake lock            # один раз, закоммитить flake.lock
nix develop
tools/doctor.py --print-lock   # внести ovmf_*.sha256 в [profile nix]
make && make test && make repro-check
# после успеха: status = verified в [profile nix]
```

Rust в M0 не используется: ядро и загрузчик — freestanding C17 и ASM
(ARCHITECTURE.md §14).

## 4. Сборка и образ

Флаги (см. `Makefile`): `-std=c17 -ffreestanding -fno-builtin -nostdlibinc
-fno-stack-protector -mno-red-zone -mgeneral-regs-only -Wall -Wextra -Werror`.
Загрузчик дополнительно `-mno-stack-arg-probe`, линкуется `lld-link
/subsystem:efi_application /entry:efi_main /nodefaultlib /Brepro`. Ядро —
`-O2 -g -fno-pic`, `ld.lld -T kernel/arch/x86_64/kernel.ld --build-id=none`,
символы отладки остаются в `out/kernel.elf`.

`tools/image/mkimage.py` пишет образ сам, без mtools/mkfs: 64 MiB, GPT
(фиксированные GUID диска и раздела), один раздел EFI System Partition с
LBA 2048, FAT32 с кластерами 512 байт, только имена 8.3:

```text
\EFI\BOOT\BOOTX64.EFI     загрузчик (путь съёмного носителя по умолчанию)
\NANOX\KERNEL.ELF         ядро
\NANOX\INITRD.IMG         initramfs, cpio newc (с M1, см. m1-kernel.md)
\NANOX\MANIFEST.BIN       размеры и SHA-256 ядра и initramfs (boot-info.md §7)
\NANOX\CMDLINE.TXT        командная строка ядра (в образе make — пустая)
```

Метки времени FAT берутся из `SOURCE_DATE_EPOCH`, если переменная задана,
иначе 1980-01-01 00:00:00. Ключи `--omit-kernel`, `--corrupt-kernel`,
`--omit-initrd`, `--corrupt-initrd` использует только harness для сценариев
отказа.

## 5. Воспроизводимость

Меры: `-ffile-prefix-map=$(CURDIR)=.` (в объектах и DWARF нет абсолютных
путей), `/Brepro` (метка времени PE — хеш содержимого), `--build-id=none`,
детерминированный писатель образа без внешнего состояния.

`make repro-check` (`tools/repro_check.py`) копирует текущее дерево
(отслеживаемые и неигнорируемые файлы) в два новых каталога с разными
абсолютными путями, собирает `make all` там и в рабочем дереве и требует
побайтового совпадения `BOOTX64.EFI`, `kernel.elf`, `initrd.img` (с M1) и
`nanox.img` во всех трёх сборках. Отчёт: `out/repro-check.json`. Отрицательный контроль при подготовке
M0: сборка с пустым `REPRO_FLAGS` в двух путях дала разные `kernel.elf`
(абсолютный путь в DWARF), то есть проверка обнаруживает такие различия.

## 6. Headless harness

`tools/bench/harness.py` (только стандартная библиотека Python). Сценарии —
`tests/qemu/scenarios.json`. Ниже — сценарии M0; сценарии M1 перечислены в
[m1-kernel.md](m1-kernel.md#сценарии-стенда).

| Сценарий | Образ | Ожидание |
| --- | --- | --- |
| `normal` | пустая командная строка | PASS, статус 33; в логе SHA-256 ядра, совпадающий с хешем `out/kernel.elf` на хосте, и адрес RSDP |
| `fail` | `nanox.test=fail` | FAIL `test_fail`, статус 35 |
| `unknown-test` | `nanox.test=bogus` | FAIL `test_fail`, статус 35 |
| `panic` | `nanox.test=panic` | FAIL `panic`, статус 37 |
| `hang` | `nanox.test=hang`, таймаут 20 с | FAIL `timeout`; в логе есть `NANOX: kernel_main` и `NANOX: test hang` |
| `missing-kernel` | без `KERNEL.ELF` | FAIL `loader_error` `E_KERNEL_OPEN`, статус 39 |
| `corrupt-kernel` | один байт ядра инвертирован после записи манифеста | FAIL `loader_error` `E_KERNEL_HASH`, статус 39 |
| `framebuffer` | каноническая конфигурация + `-device VGA` | PASS; строка `NANOX: bootinfo framebuffer WxH ...` |

Таймаут по умолчанию 60 с. `make test` успешен, только если каждый сценарий дал
ожидаемый вердикт, класс отказа, код ошибки загрузчика и строки лога.

### Правила вердикта

Нужны **и маркер, и код выхода**:

- **PASS**: статус 33, ровно одна строка `NANOX: TEST PASS`, ей предшествуют
  по порядку `NANOX: loader start`, `NANOX: loader exit_boot_services ok`,
  `NANOX: kernel_main`, `NANOX: bootinfo ok`, и нет строк `TEST FAIL`,
  `PANIC`, `LOADER ERROR`.
- Иначе **FAIL** с классом: `test_fail` (35 + `TEST FAIL`), `panic`
  (37 + `PANIC`), `loader_error` (39 + `LOADER ERROR`), `timeout` (процесс
  завершён harness; маркер PASS в логе не спасает), `inconsistent` (статус и
  маркеры не согласованы, например 33 без `TEST PASS`), `unexpected_exit`
  (любой другой статус, например 0 после тройной ошибки).

Маркер — строка, начинающаяся с `NANOX: ` (перед сравнением удаляются `\r` и
ANSI-последовательности вывода OVMF). Эти правила покрыты
`tests/host/test_harness.py`.

### Serial-маркеры

| Строка | Кто печатает |
| --- | --- |
| `NANOX: loader start` | загрузчик, первая строка |
| `NANOX: loader kernel sha256 ok <hex>` | загрузчик после проверки манифеста |
| `NANOX: loader exit_boot_services ok regions=N` | загрузчик после `ExitBootServices` |
| `NANOX: loader jump entry=... bootinfo=... stack_top=...` | загрузчик, последняя строка |
| `NANOX: LOADER ERROR <ИМЯ> (<код>): ...` | загрузчик, отказ |
| `NANOX: kernel_main bootinfo=<адрес>` | ядро, первая строка |
| `NANOX: bootinfo ok version=... regions=... usable_bytes=...` | ядро после проверки boot info |
| `NANOX: mmap NN <начало>-<конец> <тип>` | ядро, карта памяти |
| `NANOX: kernel sha256 <hex>` | ядро, хеш из boot info |
| `NANOX: TEST PASS` / `NANOX: TEST FAIL <причина>` | ядро, вердикт |
| `NANOX: PANIC <сообщение>` | ядро, panic |

### Коды выхода

| Значение в порт `0xF4` | Статус QEMU | Смысл |
| ---: | ---: | --- |
| `0x10` | 33 | TEST PASS |
| `0x11` | 35 | TEST FAIL |
| `0x12` | 37 | PANIC |
| `0x13` | 39 | LOADER ERROR |
| — | 0 | сброс/выключение без isa-debug-exit |
| — | нет | таймаут, процесс остановлен harness |

## 7. Запись запуска

Каждый запуск (`make test`, `make run`) создаёт
`out/runs/<UTC-время>-<сценарий>/`:

| Файл | Содержимое |
| --- | --- |
| `record.json` | запись `nanox.run-record.v1` |
| `serial.log` | сырой вывод COM1 |
| `qemu-output.log` | stdout/stderr QEMU |

Поля `record.json`:

| Поле | Содержимое |
| --- | --- |
| `scenario`, `description`, `started_utc`, `duration_s` | что и когда запущено |
| `source.git_rev`, `source.git_dirty`, `source.git_dirty_paths` | исходная ревизия и незакоммиченные изменения |
| `qemu.argv`, `qemu.command_line`, `qemu.timeout_s` | полная командная строка |
| `image_spec` | параметры сценарного образа (cmdline, fault injection) |
| `artifacts.{image,loader_efi,kernel_elf,ovmf_code,ovmf_vars_template}` | путь, SHA-256, размер |
| `toolchain.profile`, `toolchain.lock_ok`, `toolchain.versions` | фактические версии инструментов и результат сверки с lock |
| `host` | ОС и версия Python хоста |
| `result.exit_status`, `result.timed_out`, `result.killed_returncode` | статус выхода (`null` при таймауте) |
| `serial.sha256`, `serial.markers`, `serial.raw` | хеш, маркеры и полный текст serial-лога |
| `verdict`, `failure_class`, `loader_error`, `reason` | вердикт harness |
| `expected`, `expectation_met`, `expectation_problems` | ожидание сценария и расхождения |

`make test` дополнительно пишет `out/runs/<время>-suite/summary.json` со
списком запусков. Копия `OVMF_VARS.fd` удаляется после запуска; в записи
остаётся хеш шаблона.

## 8. Отладка через GDB

Ядро собрано с `-g`, слинковано и загружено по адресу `0x200000`, виртуальный
адрес равен физическому (identity mapping UEFI), поэтому символы ELF годятся
без смещения.

```sh
make debug                      # терминал 1: QEMU остановлен до прошивки, GDB stub на :1234
gdb -x tools/gdb/nanox.gdb      # терминал 2
```

`tools/gdb/nanox.gdb` загружает `out/kernel.elf`, подключается к
`localhost:1234`, ставит `hbreak kernel_main` и продолжает; на остановке
печатает `bi->magic` и `*bi`. Аппаратная точка останова выбрана потому, что
загрузчик копирует ядро в память уже после подключения GDB: под KVM
программная точка останова была бы затёрта этим копированием (под TCG
работают обе).

`make debug-check` выполняет ту же процедуру в batch-режиме на свободном
порту и проверяет, что GDB остановился в `kernel_main`, `RIP` указывает внутрь
`kernel_main`, а по адресу из `RDI` лежит magic boot info. Запись:
`out/runs/<время>-gdb/record.json` и `gdb.log`.

## 9. Что проверено при подготовке M0

Эталонная среда: Ubuntu 24.04, x86-64, без KVM. Выполнено из рабочего дерева
и из свежего `git clone`:

- `make doctor` — все пункты профиля `ubuntu-24.04` совпали;
- `make` — сборка без предупреждений (`-Werror`);
- `make host-test` — C-тесты валидатора boot info, SHA-256, ELF-плана и
  перевода карты памяти (UBSan в режиме trap); дополнительно однократно
  собраны gcc с `-fsanitize=address,undefined`;
- `make py-test` — правила вердикта harness, GPT/FAT32 и обратное чтение
  образа через mtools;
- `make qemu-test` — все 8 сценариев дали ожидаемый результат;
- `make repro-check` — три сборки побайтово совпали;
- `make debug-check` — GDB остановился в `kernel_main`.

Не проверено: окружение `flake.nix`, запуск под KVM, реальное оборудование.
Результат QEMU-теста не доказывает работу на произвольном железе
(ARCHITECTURE.md §17).
