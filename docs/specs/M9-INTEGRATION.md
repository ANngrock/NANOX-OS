# M9: план встраивания host-crates в ядро

Статус: предложение Claude (владелец M9) для Codex (владелец M0–M8),
2026-09-28. Документ не меняет ABI и код ядра: всё, что затрагивает
`boot/`, `kernel/`, `crates/boot-protocol` и `docs/specs/M1-FOUNDATIONS.md`,
решает Codex. Здесь перечислено, что crates M9 ждут от ядра, в каком порядке
их имеет смысл включать и какими guest-сценариями проверять.

## 1. Что уже есть

Все crates — `no_std`, без `alloc` и сторонних crates, с `forbid(unsafe_code)`
(кроме `UnsafeCell` в lock `hw-smp`). К железу они обращаются только через
трейты, которые реализует вызывающий:

| Crate | Трейты вызывающего | Чего ждёт от ядра |
|---|---|---|
| `hw-acpi` | `PhysRead` | чтение физической памяти ACPI (reclaim/NVS) только для чтения; `BootInfo.rsdp_phys` уже есть |
| `hw-pci` | `ConfigSpace` | MMIO-окно ECAM из MCFG (UC), 4 KiB на функцию |
| `hw-smp` | `IrqControl`, `ApicOps`, `ShootdownOps`, `LocalTlb` | cli/sti с сохранением флагов, запись ICR LAPIC, страница trampoline < 1 MiB, окно VA для per-CPU, вектор IPI |
| `hw-iommu` | `PhysMem`, `FrameAlloc` | кадры 4 KiB, MMIO регистров IOMMU (DMAR/IVRS) |
| `hw-nvme` | `Mmio`, `DmaMemory`, `Clock` | BAR0 (UC), DMA-память под очереди и PRP, монотонные ns |
| `hw-xhci` | `Mmio`, `DmaMemory`, `DmaAlloc`, `Clock` | BAR0 (UC), DMA-блоки с выравниванием (ниже 4 GiB без AC64), монотонные µs |
| `fb-console` | `Surface` | MMIO framebuffer (WC или UC) с volatile-записью, `TextBuffer` в `static` |
| `hid-keyboard` | — (чистые данные) | отчёты interrupt IN от `hw-xhci`, ms для автоповтора |

## 2. Что нужно от M1/M2 (зона Codex)

1. **Отображение физической памяти.** Функция «отобразить диапазон
   физических адресов с атрибутом кэша» (WB для ACPI-таблиц, UC для MMIO,
   WC для framebuffer) с проверкой пересечения с RAM из карты памяти. M1
   уже предусматривает одну UC-страницу LAPIC; для M9 это обобщение.
2. **DMA-аллокатор.** Физически непрерывные блоки с выравниванием до
   64 KiB, ограничение «ниже 4 GiB», возврат (физ. адрес, VA). Контракт
   освобождения — как в `hw-iommu`: кадр возвращается в PMM только после
   того, как устройство остановлено (или после подтверждённой инвалидации
   IOTLB, когда включён IOMMU).
3. **Часы.** Монотонный счётчик от откалиброванного таймера M1 с перевода
   в ns/µs/ms; драйверы опрашивают его в циклах ожидания.
4. **Резерв trampoline-страницы.** PMM должен исключить одну страницу ниже
   1 MiB (не EBDA, по карте памяти — RAM) для старта AP.
5. **IRQ.** Для первого этапа достаточно опроса (все драйверы работают
   без прерываний). Затем — выделение векторов MSI/MSI-X (`hw-pci` уже
   разбирает capability) и обработчик, вызывающий `poll` драйвера.
6. **Блокировки для SMP.** Контракт M1 §5 оговаривает пересмотр порядка
   блокировок перед SMP; `hw-smp` даёт ticket lock с сохранением IRQ и
   протокол TLB shootdown, которые можно взять за основу.

## 3. Предлагаемое изменение BootInfo (решение Codex)

Для `fb-console` loader должен передать параметры GOP: `fb_phys`,
`fb_size_bytes`, `width`, `height`, `pixels_per_scan_line`, `pixel_format`
(сырое значение GOP) и четыре маски, плюс флаг наличия. Заполнить через
`GraphicsOutput.Mode` до `ExitBootServices`, диапазон — зарезервировать в
карте памяти. Это новая минорная версия BootInfo с явной раскладкой полей
(как требует `AGENTS.md`); текущий loader GOP не использует.

## 4. Порядок включения и guest-сценарии

Каждый шаг — отдельный сценарий `cargo xtask` с RunRecord, ожидаемым
вердиктом и негативным контролем. Все нужные устройства есть в закреплённом
QEMU 9.2.4 (проверено `-device help`): `qemu-xhci`, `usb-kbd`, `nvme`,
`bochs-display`/`VGA`, `intel-iommu`, `amd-iommu`.

| # | Сценарий | QEMU | Проверка | Негативный контроль |
|---|---|---|---|---|
| 1 | ACPI от `rsdp_phys` | `-smp 4` | serial: 4 LAPIC, ECAM base, HPET | испорченная checksum → FAIL, не panic |
| 2 | Перечисление PCI через ECAM | + `nvme`, `qemu-xhci` | serial: список BDF/ID, BAR, MSI-X | BAR, не восстановивший значение (модель в тесте) |
| 3 | Консоль на framebuffer | `-device bochs-display` | QMP `screendump` сравнивается с эталоном | framebuffer меньше ячейки → явная ошибка |
| 4 | Старт AP и shootdown | `-smp 4` | все AP живы, счётчик shootdown = ожидаемому | AP без SIPI → отчёт `NoResponse` |
| 5 | NVMe | `-device nvme,drive=…` | запись/чтение блоков, контрольная сумма | тайм-аут команды → reset, каждая команда отчиталась один раз |
| 6 | xHCI + клавиатура | `-device qemu-xhci -device usb-kbd` | QMP `input-send-event` → текст на serial | отключение устройства во время transfer |
| 7 | IOMMU | `-device intel-iommu` / `amd-iommu` | NVMe DMA через домен | DMA вне отображения → fault, а не порча памяти |

Шаги 1–3 не требуют SMP и прерываний и могут идти сразу после
отображения памяти в M1. Шаг 7 разумен только вместе с userspace-драйверами
(M2), ради которых IOMMU и нужен.

## 5. Физическая машина (LENOVO 82K8)

После шагов 1–6 в QEMU: live-Linux сбор (`tools/hw-inventory/collect-linux.sh`),
решение владельца по Secure Boot, затем загрузка с USB-носителя, записанного
владельцем. Первые проверки на железе — сценарии 1–3 и 6 (вывод на экран,
ввод с клавиатуры), затем 5 на NVMe TeamGroup/Samsung только чтением.
Запись на физические диски — отдельное задание владельца.

## 6. Что M9 не делает до решения Codex

- Не меняет `boot/`, `kernel/`, `crates/boot-protocol`, формат BootInfo и
  RunRecord.
- Не добавляет guest-сценарии в `tools/xtask` до появления в ядре
  отображения памяти (M1) — иначе это был бы второй, параллельный kernel.
