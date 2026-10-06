# План: остаток проекта (зона Claude: M9 → M10 → R → M11)

Обновлено: 2026-10-01. Проверка каждого пункта — `m9-checks.sh --replay` (fmt,
clippy, build-none, тесты, xtask-test) и запись в PR. Критерий ROADMAP не
закрывается, пока нет исполняемого доказательства именно на том, что в нём
написано (физическое железо, ядро NANOX, самосборка внутри ОС).

## Что можно сделать без железа и без ядра Codex

- [x] **M11 (новый, последний этап): режим сервера и Proxmox** — host-срез.
  - [x] Запись в ROADMAP и спецификация `docs/specs/M11-SERVER.md`.
  - [x] `crates/qga`: протокол QEMU Guest Agent (гостевая сторона), ответы
        побайтно сверены с настоящим `qemu-ga` 9.2.4.
  - [x] `crates/svc`: supervisor служб, режимы desktop/hybrid/server и
        переключатель, бюджеты ресурсов, детерминированные тесты.
  - [x] `crates/vswitch`: виртуальный L2-коммутатор для служб.
  - [x] `tools/proxmox`: командная строка, как у Proxmox VE (q35, OVMF,
        virtio-scsi/net/serial, watchdog), загрузка образа M0 в ней.
- [x] **R — исследовательские результаты** (`docs/research/`, сводка — README).
  - [x] Ограниченная IR и две согласованные реализации (`crates/nanox-ir`,
        эталон `tools/nir`).
  - [x] Проверяемые свойства компонентов с версиями (`crates/proofs`: полный
        перебор ограниченных областей, аттестации по хешу исходников).
  - [x] Воспроизводимость сценария VMM с записанными входами.
  - [x] Планирование по зависимостям против baseline (`crates/dep-sched`).
  - [x] Решение по собственному ISA/compiler: измерение и отрицательный
        результат (не делать).
  - [x] Отчёты с ограничениями, затратами и отрицательными результатами.
- [x] **M10, что не зависит от ядра.**
  - [x] Native userspace target и runtime-слой: спецификация
        `docs/specs/M10-NATIVE.md`, `crates/runtime`, `user/`, `tools/native/`.
  - [x] Выбор пути к rustc/cargo внутри NANOX: владелец выбрал оба — изолированная
        служба (путь B) и кросс-сборка (путь C), порт `std` (путь A) позже.
  - [x] Путь B на хосте: `crates/linux-compat` — ядро личности Linux (политика,
        пути, дескрипторы, память, обработчики 87 вызовов), тесты, мутации.
  - [x] Путь C на хосте: `tools/native/pack.py` (две сборки в разных каталогах,
        контракт ELF), формат NXPK, читатель в `crates/runtime`.
  - [ ] Путь B и C внутри NANOX (ядро с userspace, `Backend` над объектами,
        перехват `syscall`, загрузчик Linux ELF): [M10-ROUTES](../docs/specs/M10-ROUTES.md), §2.6.
- [x] **M11: окно сервера** (`crates/serverwin`, [M11-WINDOW](../docs/specs/M11-WINDOW.md)),
      измерение требований Linux-гостя (`tools/hostguest`).
- [ ] **VMM для гостя Linux** (M11-WINDOW §5; устройства в `crates/vmm-devices`, PR #13):
  - [x] Шаг 2: UART, 8259+ELCR, RTC, легаси-порты, IOAPIC, HPET, i8042, ACPI PM, шина `Machine`.
  - [x] Шаг 3: шина PCI (механизм №1, ECAM), транспорт virtio 1.x, `virtio-blk`, `virtio-net`.
  - [x] Шаг 4: `virtio-gpu` (2D) за трейтом `Scanout`.
  - [x] Шаг 6: канал агента `virtio-console`.
  - [x] Шаг 5: ввод `virtio-input` (клавиатура и планшет).
  - [x] PIT каналы 0–2 и IRQ0 (частичных строк в `map` больше нет).
  - [x] Шаг 1, часть: ACPI-таблицы платформы (RSDP…DSDT).
  - [x] Свести три ветки в `claude/m10-vmm-legacy`, мутации, STATUS, пуш, PR #13.
  - [x] Цикл vCPU на `Machine` (`hw-svm::platform_vm`), прямая загрузка Linux
        (`guest-boot::linux`), `setup_linux_boot`.
  - [x] Linux 7.0.2-6-pve под VMM NANOX до `NANOX_GUEST_REPORT_END` (svm-probe, QEMU TCG).
  - [x] Согласованное время гостя (TSC по виртуальному времени, RDTSC перехвачен).
  - [x] Диск гостя в зонде (образ в памяти через `virtio-blk`).
  - [x] Экран гостя: кадровый буфер в `screen_info`, консоль Linux, `screen.png`.
  - [x] Канал агента: приветствие гостя через `virtio-console` и ответ VMM.
  - [x] Сеть: `eth0` гостя, ARP и ping до адреса хоста (`vswitch::endpoint`).
  - [x] Ввод: клавиатура PS/2 (исправлен фронт IRQ1 у i8042), набранная строка у гостя.
  - [x] Экран NANOX в зонде: окно сервера с гостем на дисплее UEFI GOP (`crates/canvas`).
  - [x] Живое окно для владельца: `run.py --show` (QEMU с GTK через WSLg, `.#qemu-display`).
  - [x] Интерактивная оболочка busybox в госте, ввод из окна; проверка `--shell-test` через QMP.
  - [ ] Запуск на железе и под ядром NANOX (VMRUN в ядре — Codex); вложенный SVM;
        протокол агента (QGA); сеть наружу через vswitch.

## Нельзя закрыть здесь (причина)

- M9: boot/ACPI/PCI/драйверы на ноутбуке, SMP на железе, IOMMU, сброс
  устройств, GPU — нужен физический LENOVO 82K8 и отдельное задание владельца.
- M10: compiler и сборка внутри NANOX, самопересборка образа, обновление с
  recovery внутри ОС, цепочка с M8 — нужны M2–M8 (Codex).
- M11 на деле (Proxmox-хост, soak сутки, реальная служба снаружи, драйверы
  virtio-serial/net, ACPI shutdown, интеграция supervisor/vswitch в ядро):
  нужны ядро M2–M5 и Proxmox VE.

## Проверка и публикация

- Ветки цепочкой: `claude/m11-server` (от `claude/m10-vmm`), затем
  `claude/r-research`, `claude/m10-native`, `claude/m11-window`,
  `claude/m10-routes`; PR к предыдущей ветке.
