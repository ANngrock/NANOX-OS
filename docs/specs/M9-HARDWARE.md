# M9 — физическое железо, SMP и устройства: контракт и срезы

Статус: рабочий контракт, 2026-09-26; §3.5–3.7 добавлены 2026-09-27. Владелец M9 — Claude (M0–M8 ведёт
Codex). Ни один критерий M9 в [ROADMAP](../ROADMAP.md) этим документом не
закрывается: host-тесты и снятые таблицы не являются проверкой на железе.

## 1. Границы и порядок

ROADMAP ставит M9 после M2 и драйверов M4–M5. Rust-линия сейчас на M0, ядро
(M1–M2) пишет Codex. Поэтому M9 начинается, как M5, с host-проверяемых
`no_std` crates без правок `kernel/` и `boot/`:

| Crate | Содержание | Guest-интеграция |
|---|---|---|
| `crates/hw-acpi` | RSDP v0/v2, RSDT/XSDT, MADT, FADT, HPET, MCFG, DMAR, IVRS | после M1 (mapping физической памяти) |
| `crates/hw-pci` | ECAM, заголовки, BAR, capability-списки, MSI/MSI-X, обход шин | после M1 (MMIO mapping) |
| `crates/hw-smp` | топология CPU, план старта AP, per-CPU layout, lock, TLB shootdown | после M1 (IDT, LAPIC, page tables) |
| `crates/hw-iommu` | VT-d и AMD-Vi структуры, трансляция, жизненный цикл DMA mapping | после M1/M2 и драйверов M4–M5 |
| `crates/hw-nvme` | NVMe: регистры, init, очереди, Identify, I/O, PRP, reset/recovery | после M1/M2 (MMIO, DMA, IRQ) |
| `crates/hw-xhci` | xHCI: регистры, handoff, init, кольца TRB, порты, Address Device, дескрипторы, recovery | после M1/M2 |
| `crates/fb-console` | текстовая консоль на GOP framebuffer, собственный шрифт 8×16 | после передачи framebuffer в BootInfo (решение Codex) |
| `crates/hid-keyboard` | boot-отчёты USB-клавиатуры → события, автоповтор, раскладки US/ЙЦУКЕН, LED | вместе с `hw-xhci` |

Правила (в дополнение к `AGENTS.md`):

- Без сторонних crates, без `alloc`. `#![forbid(unsafe_code)]`, кроме
  `hw-smp`, где `unsafe` допускается только для `UnsafeCell` в lock с
  локальным `SAFETY`-обоснованием.
- Доступ к железу — только через трейты, которые реализует вызывающий
  (ядро или тестовая модель). Crates не читают физическую память сами.
- Разбор проверяет длину, контрольную сумму, переполнение и пересечения до
  чтения поля. Некорректный вход — ошибка, не panic и не частичный результат.
- Каждый crate собирается для `x86_64-unknown-none` и проходит clippy
  `-D warnings`. Тесты проверяют поведение на границах и отказах, а не
  повторяют код.

## 2. Профили

### 2.1. QEMU-профиль M9 (виртуальный)

База — профиль M0 (`docs/specs/machine-profile.toml`), отличия: 4 vCPU;
варианты с `-device intel-iommu` (DMAR) и `-device amd-iommu` (IVRS).
ACPI-таблицы, которые OVMF публикует для этих машин, сняты
`tools/acpi-capture/capture.py` в `tests/fixtures/acpi/q35-smp4*/`
(с `manifest.json`: argv, хеши QEMU/OVMF, физические адреса).
Наблюдение: QEMU q35 публикует RSDP ревизии 0 и только 32-битную RSDT.

### 2.2. Физический профиль (выбран владельцем 2026-09-27)

Рабочий ПК пользователя, данные получены только чтением из Windows:

- LENOVO 82K8, плата LNVNB161216, BIOS HACN46WW (2024-11-14), UEFI.
- AMD Ryzen 7 5800H (Zen 3, Cezanne), 8 ядер / 16 потоков; IOMMU — AMD-Vi (IVRS).
- GPU: встроенный AMD Radeon (1002:1638) и NVIDIA RTX 3060 Laptop (10DE:2560).
- Накопители: NVMe Phison (126F:2263) и Samsung (144D:A808).
- Сеть: только Wi-Fi Intel AX200 / Killer AX1650x (8086:2723); проводного
  Ethernet нет.
- USB: xHCI AMD (1022:1639) x2; SD: BayHub (1217:8621).

ACPI-таблицы APIC, FACP, HPET, MCFG, IVRS — в `tests/fixtures/acpi/lenovo-82k8/`.
MSDM (ключ продукта), VBIOS и AML намеренно не сохранены.

Риски кандидата: нет COM-порта (диагностика потребует framebuffer или USB
debug), нет virtio — нужны драйверы NVMe и xHCI, сеть только через Wi-Fi
(прошивка и драйвер iwlwifi-класса — отдельная большая работа). Загрузка
NANOX на этой машине требует записи USB-носителя пользователем и отдельного
задания; агенты физические диски не трогают.

## 3. Контракты crates

### 3.1. `hw-acpi`

Вход — байтовые срезы таблиц и трейт чтения физической памяти для обхода
от RSDP. Выход — типизированные представления без копирования в кучу:
итераторы записей MADT (LAPIC, x2APIC, IOAPIC, ISO, NMI/LAPIC NMI, LAPIC
address override), MCFG-сегменты, FADT (версия, флаги, reset register,
PM timer, X_-поля с приоритетом над 32-битными), HPET, DMAR (DRHD, RMRR,
ATSR, device scope), IVRS (IVHD 10h/11h/40h, IVMD). Отказы: сигнатура,
длина меньше заголовка или больше буфера, контрольная сумма, усечённая или
нулевая длина записи, выход записи за таблицу, цикл/повтор указателей
RSDT/XSDT, ревизии вне поддерживаемых.

### 3.2. `hw-pci`

Трейт доступа к конфигурационному пространству; вычисление ECAM-адреса из
MCFG-сегмента с проверкой диапазона шин; заголовки типа 0/1; декодирование
и протокол измерения BAR (32/64-bit, prefetchable, I/O) с восстановлением
исходного значения; обход capability (обычных и extended) с защитой от
циклов и выхода за 256/4096 байт; MSI и MSI-X; рекурсивный обход мостов с
ограничением глубины и проверкой пересечения/повтора номеров шин.

### 3.3. `hw-smp`

Топология из списка APIC ID (enabled/online-capable, дубликаты, BSP);
план старта AP: выбор страницы trampoline ниже 1 MiB, последовательность
INIT-SIPI-SIPI с тайм-аутами как конечный автомат, отчёт о неответивших
CPU; per-CPU layout без пересечений; spinlock с сохранением IRQ-состояния
через трейт; протокол TLB shootdown (поколение, маска целей, подтверждения,
тайм-аут, отказ от повторного входа). Host-тесты используют настоящие
потоки `std` как модель CPU.

### 3.4. `hw-iommu`

VT-d: root/context entries, second-level page tables, построение в кадры,
выданные вызывающим. AMD-Vi: device table entry и page tables. Общий
жизненный цикл mapping: `map → in-flight DMA → quiesce device → unmap →
invalidate IOTLB → wait completion → free frame`; кадр нельзя освободить
раньше подтверждённой инвалидации. Host-модель транслирует DMA и выдаёт
fault на неотображённый адрес, запрет записи и доступ после unmap.

### 3.5. `hw-nvme`

Драйверное ядро NVMe (Base Specification 2.0) для двух накопителей целевого
ноутбука: регистры CAP/CC/CSTS, инициализация как автомат с тайм-аутами по
CAP.TO, admin и I/O очереди с phase tag, Identify, Read/Write/Flush, PRP и
PRP list с учётом MDTS. Recovery: тайм-аут команды → abort или reset
контроллера; при reset каждая незавершённая команда завершается явной
ошибкой ровно один раз; CSTS.CFS в любой фазе; shutdown через CC.SHN.
Серийный номер и модель из Identify — байты для вызывающего, не для логов.

### 3.6. `hw-xhci`

Драйверное ядро xHCI 1.2 для двух контроллеров USB ноутбука: BIOS/OS handoff,
reset и запуск контроллера, DCBAA и scratchpad, command/transfer/event rings
с cycle bit и Link TRB, PORTSC с RW1C-дисциплиной, reset порта, Enable Slot,
Address Device, control transfer GET_DESCRIPTOR, разбор дескрипторов,
Configure Endpoint для interrupt IN. Recovery: Command Abort, stall →
Reset Endpoint, отключение устройства с незавершёнными операциями, HSE →
полный reset.

### 3.7. `fb-console`

Текстовая консоль на линейном framebuffer UEFI GOP (RGBX, BGRX, bitmask;
BltOnly отвергается) с собственным шрифтом 8×16 для ASCII — ранняя
диагностика на машине без COM-порта. Описание framebuffer проверяется до
записи; скролл без чтения медленного MMIO; аварийный вывод, не зависящий от
состояния консоли. Для интеграции loader должен передать параметры GOP в
BootInfo — это изменение ABI в зоне Codex.

План встраивания в ядро и guest-сценарии — [M9-INTEGRATION](M9-INTEGRATION.md).

## 4. Критерии ROADMAP и что их закрывает

| Критерий | Что нужно | Host-срезы дают |
|---|---|---|
| Выбран hardware profile | решение владельца + запись устройств/firmware | выбран LENOVO 82K8 (§2.2); запись из Windows, live-Linux сбор отложен |
| Boot, ACPI/PCI, IRQ на железе | загрузка на выбранной машине | парсеры и модели |
| SMP, per-CPU, TLB shootdown | guest-сценарий с `-smp 4` и на железе | протоколы и потоковые тесты |
| DMA/IOMMU | guest с `intel-iommu`/`amd-iommu`, затем железо | структуры и модель трансляции |
| Reset/recovery | фактические тесты устройств | модели NVMe и xHCI: reset с незавершёнными командами, stall, HSE, CFS |
| GPU отдельно | после выбора устройства | — |

## 5. Проверка среза

В WSL, в worktree `~/src/NANOX-OS-m9`, внутри `nix develop --offline`:

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked -p hw-acpi -p hw-pci -p hw-smp -p hw-iommu -p hw-nvme -p hw-xhci -p fb-console --all-targets -- -D warnings
cargo build --offline --locked -p hw-acpi -p hw-pci -p hw-smp -p hw-iommu -p hw-nvme -p hw-xhci -p fb-console --target x86_64-unknown-none
cargo test --offline --locked -p hw-acpi -p hw-pci -p hw-smp -p hw-iommu -p hw-nvme -p hw-xhci -p fb-console
git diff --check
```
