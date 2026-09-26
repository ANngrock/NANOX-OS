# Физические hardware-профили NANOX

Первый критерий M9 ([ROADMAP](../ROADMAP.md)): «выбран один точный hardware
profile; записаны устройства/firmware». Здесь хранятся записи профилей и
доказательства, из которых они построены. Инструменты — в
[`tools/hw-inventory/`](../../tools/hw-inventory/), контракт M9 —
[M9-HARDWARE](../specs/M9-HARDWARE.md).

| Файл | Что это |
|---|---|
| `<id>.toml` | запись профиля: система, BIOS, CPU, RAM, PCI-функции с ролью и статусом поддержки, накопители, ACPI-таблицы с хешами и разбором, IOMMU, риски |
| `inventory/<id>-<os>-<дата>.json` | сырой вывод сборщика, на который ссылается `[source]` записи (путь и sha256) |

Текущие записи: [`lenovo-82k8.toml`](lenovo-82k8.toml) — кандидат, собран
из Windows без прав администратора.

## Как собрать

Все сборщики только читают; ничего не устанавливают, не меняют настройки
firmware/ОС и не пишут на диски, кроме своего файла/каталога вывода.

Windows (без администратора):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tools\hw-inventory\collect-windows.ps1 -OutFile inventory.json
```

Live Linux на целевой машине (офлайн; нужны `pciutils`, `dmidecode`, root):

```sh
sudo ./tools/hw-inventory/collect-linux.sh OUT_DIR          # полный сбор, иначе ясная ошибка
./tools/hw-inventory/collect-linux.sh --partial OUT_DIR     # без root: что доступно, пропуски в collect.log
```

Linux-сбор дополнительно сохраняет `lspci -nnvvv`/`-xxxx`, отфильтрованный
`dmidecode`, строки `dmesg` про IOMMU/SMP, IOMMU-группы и бинарники
разрешённых ACPI-таблиц. Он видит то, что Windows скрывает: PCI-функцию
AMD IOMMU (её занимает Hyper-V), config space, IOMMU-группы.

Запись профиля и сверка (в `nix develop`, из корня репозитория):

```sh
python3 tools/hw-inventory/profile.py INVENTORY --id lenovo-82k8 \
    --out docs/hardware/lenovo-82k8.toml --inventory-ref docs/hardware/inventory/<файл>.json
python3 tools/hw-inventory/profile.py INVENTORY --check docs/hardware/lenovo-82k8.toml
python3 -m unittest discover -s tools/hw-inventory -v
```

`INVENTORY` — JSON Windows-сборщика или каталог `collect-linux.sh`.
`--check` завершается 0, если факты совпали; 1 — появилось/исчезло
PCI-устройство, изменились ID/ревизия, BIOS (версия/дата), CPU, объём RAM,
firmware накопителей, режим прошивки/Secure Boot или хеш/набор ACPI-таблиц;
2 — некорректный вход. Поля, которые новый сбор не смог увидеть (ACPI без
root, Secure Boot `unknown`), печатаются как «not observed» и различием не
считаются. Сверка Windows-записи со сбором из Linux ожидаемо покажет новую
функцию `00:00.2` (AMD IOMMU).

## Что редактируется руками

Генератор заполняет факты и `detected_risks` сам и перезаписывает их.
Человек меняет только:

- `status`, `confirmed_by`, `confirmed_on`;
- `notes` и `risks` — свободные строки;
- у `[[pci]]`: `nanox` и `note`.

Повторная генерация сохраняет эти поля (PCI — по BDF и vendor:device).
Запись со статусом `confirmed` не перезаписывается другими фактами без
`--force`.

## Статусы

- Профиль: `candidate` — собран и записан, владелец машины выбор не
  подтвердил; `confirmed` — владелец подтвердил, что NANOX целится именно
  в эту машину с этой версией BIOS. **Подтверждает только владелец**; ни
  инструмент, ни агент не ставят `confirmed`. Смена BIOS или устройств
  после подтверждения обнаруживается `--check` и требует нового решения.
- PCI-функция, поле `nanox`: `none` — поддержки нет и не запланирована
  в M9; `planned` — нужна для пути M9 (PCI-мосты, IOMMU, NVMe, xHCI);
  `driver` — драйвер NANOX существует и проверен на этой машине.
  Инструмент никогда не ставит `driver`.

## Что не сохраняется

Сборщики и `profile.py` не сохраняют серийные номера, UUID системы/платы,
asset tag, MAC-адреса, пути экземпляров PnP (в них бывает Device Serial
Number), имя компьютера и пользователя, ключ продукта (ACPI MSDM),
образ VBIOS (VFCT) и AML (DSDT/SSDT). Из ACPI читаются только APIC, FACP,
HPET, MCFG, IVRS, DMAR, SRAT, SLIT. Оба сборщика перед записью ищут в
выводе настоящие значения этих идентификаторов (читая их только в
память) и формы MAC/UUID/ключа продукта; `collect-windows.ps1` при
совпадении не пишет файл, `collect-linux.sh` заменяет совпадение в тексте
на `<redacted>` и завершается ошибкой, если оно осталось.
`profile.py` копирует только разрешённые поля и отказывается писать
профиль, содержащий такие значения. Тесты проверяют это на входе с
подложенными серийником, MAC, UUID, MSDM и VFCT, а также все файлы этого
каталога.
