# Статус M9: host-срезы (Claude)

Обновлено: 2026-09-26. Ветка `claude/m9-hardware`, worktree
`/home/holod/src/NANOX-OS-m9` (от `b8913fa`). Контракт —
[M9-HARDWARE](specs/M9-HARDWARE.md).

**Ни один критерий M9 в [ROADMAP](ROADMAP.md) не закрыт.** Всё ниже — host-код и
host-тесты: в guest (QEMU) и на физическом железе не исполнялось. Интеграция в
ядро ждёт M1 (mapping физической памяти и MMIO, IDT, LAPIC), который ведёт Codex.

## Что сделано

| Компонент | Путь | Тестов | Что проверено |
|---|---|---|---|
| Снятие ACPI из QEMU/OVMF | `tools/acpi-capture/capture.py` | — | таблицы q35 с 4 vCPU: без IOMMU, `intel-iommu` (DMAR), `amd-iommu` (IVRS); manifest с argv, хешами QEMU/OVMF и физическими адресами |
| ACPI-парсер | `crates/hw-acpi` | 41 | RSDP v0/v2, RSDT/XSDT, MADT, FADT, HPET, MCFG, DMAR, IVRS на всех снятых таблицах; обход от RSDP по модели памяти; негативы на каждое правило; 4000 мутаций на таблицу без panic |
| PCI/PCIe | `crates/hw-pci` | 55 | ECAM из реальных MCFG, заголовки, sizing BAR с восстановлением на всех путях, capabilities с защитой от циклов, MSI/MSI-X, обход мостов (Validate/Assign), randomized config space |
| SMP | `crates/hw-smp` | 42 | топология по реальным MADT (4 и 16 CPU), автомат INIT-SIPI-SIPI на модели APIC, per-CPU layout, ticket lock и TLB shootdown на настоящих потоках; негативный контроль «free без подтверждений» ловит нарушения |
| IOMMU | `crates/hw-iommu` | 66 | VT-d и AMD-Vi таблицы, map/unmap без частичного эффекта, жизненный цикл DMA (кадр освобождается только после подтверждённой инвалидации), модели IOMMU/IOTLB, наблюдатель записей: DTE никогда не проходит через V=0 |
| Инвентаризация | `tools/hw-inventory`, `docs/hardware/` | 20 (Python) | сборщики Windows/live-Linux с вырезанием идентификаторов; профиль `docs/hardware/lenovo-82k8.toml` (статус `candidate`) побайтно воспроизводится из сбора, `--check` без расхождений |

Все crates: `no_std`, без alloc и сторонних crates, `forbid(unsafe_code)`; в
`hw-smp` `unsafe` только для `UnsafeCell` в lock с SAFETY-обоснованиями.

## Процесс

Каждый компонент писал отдельный агент-исполнитель в своём worktree и ветке
`claude/m9-{acpi,pci,smp,iommu,inventory}`. Лидер независимо прогонял проверки
каждой ветки; для четырёх crates — отдельный агент-ревьюер (только чтение).
Подтверждённые находки исправлены авторами отдельными commit и перепроверены:

- PCI (`b7607cc`): заголовок extended capability `0xFFFFFFFF`/`0` в середине
  списка — ошибка вместо записи-мусора; отдельная ошибка secondary ≤ primary;
  тесты BAR ≥ 4 GiB. Отклонено: «порядок sizing 64-bit BAR» — совпадает с
  Linux `__pci_read_base`, PCI 3.0 §6.2.5.1 не требует иного.
- SMP (`268501b`): per-CPU флаг инициатора ставится до захвата слота — вложенный
  `start()` из прерывания сразу получает `Reentrant`; `#[must_use]` на guard;
  тесты без `thread::sleep`. Отклонено: u64-тикеты (ABA недостижим),
  Acquire во второй загрузке seqlock (fence+Relaxed — канонический паттерн).
- IOMMU (`2ee4d32`): инициализация AMD device table без окна V=0
  (pass-through), откат map при расхождении проходов, epoch u64, строгий
  decode V=0, строгая модель Mode.
- ACPI: реальных дефектов не найдено; разбор IVHD 11h в IVRS ревизии 1
  оставлен намеренно — так таблицу публикует QEMU 9.2.

## Доказательства

Финальный прогон на `claude/m9-hardware` `f65d96b`, WSL Ubuntu, `nix develop --offline`
(Rust 1.90.0, QEMU 9.2.4 `nanox-replay-exit-v1`), скрипт по §5 контракта плюс
`cargo xtask test --replay`. Логи и коды выхода:
`out/m9-checks-20260926T201826Z/` (в worktree).

- `cargo fmt --all -- --check`, `cargo clippy --offline --locked -p hw-acpi -p hw-pci
  -p hw-smp -p hw-iommu --all-targets -- -D warnings`, сборка тех же пакетов для
  `x86_64-unknown-none`, `git diff --check` — exit 0.
- `cargo test` четырёх crates: 204 прошли, 0 упали. `python3 -m unittest` в
  `tools/hw-inventory`: 20 прошли.
- `cargo xtask test --replay` — exit 0: семь QEMU-сценариев M0 без регрессий
  (`out/runs/1790453924040891303-482215-suite/suite.json`); replay PASS и FAIL:
  raw serial, verdict, disk и VARS равны
  (`out/runs/1790454055920567856-482215-pass-replay-play/`,
  `out/runs/1790454102082206245-482215-kernel-fail-replay-play/`).

## Физический профиль

LENOVO 82K8 (Legion S7 15ACH6), Ryzen 7 5800H 8C/16T, BIOS HACN46WW, AMD-Vi.
Профиль: `docs/hardware/lenovo-82k8.toml`, статус `confirmed`: владелец выбрал
эту машину 2026-09-27. Критерий 1 ROADMAP пока не отмечен: устройства и firmware
записаны только из Windows под Hyper-V; config space, функция IOMMU и
IOMMU-группы ждут live-Linux сбора, который владелец отложил.
Блокеры и риски: включён Secure Boot (неподписанный загрузчик не стартует);
нет COM-порта; нет virtio — нужны драйверы NVMe и xHCI; сеть только Wi-Fi Intel
AX200; гибридная графика AMD + NVIDIA. Не собрано: PCI config space, функция
IOMMU `00:00.2` (скрыта Hyper-V), IOMMU-группы — нужен `collect-linux.sh` с
live-USB (записывает носитель владелец).

## Следующие шаги M9

1. Отложено владельцем: Secure Boot (отключить или регистрировать ключ).
2. Отложено владельцем: live-Linux сбор `tools/hw-inventory/collect-linux.sh`,
   `profile.py --check` — закроет запись устройств для критерия 1.
3. После M1 Codex: guest-сценарии в QEMU `-smp 4` — разбор ACPI от
   `BootInfo.rsdp_phys`, перечисление PCI через ECAM, старт AP и TLB shootdown;
   затем `intel-iommu`/`amd-iommu` с драйверами M4–M5.
4. Физический прогон — только после отдельного задания владельца.
