# Ограниченный replay M0

QEMU 9.2.4 с локальным `nanox-replay-exit-v1`, `pc-q35-9.2`, OVMF 202411, TCG single thread, один CPU,
512 MiB, фиксированный RTC и `icount shift=3`. Версии и firmware hashes
заданы в [machine-profile.toml](specs/machine-profile.toml). Итоговые доказательства
последней реализации перечислены в [STATUS](STATUS.md).

Загрузочный диск подключён через `blkreplay` над новым qcow2 overlay.
CODE — readonly pflash; VARS — pflash над новым overlay исходного шаблона.
На pflash фильтр не ставится. Это конкретный проверяемый профиль M0, а не
утверждение о replay произвольных flash, DMA, устройств или hardware.

## Испытанная несовместимость

Прогон `out/runs/1790161340806702912-20226-pass-replay-record/` использовал
`blkreplay` для всех трёх block nodes. За 30 секунд не появился ни один byte
serial, trace остался нулевой длины, harness выдал TIMEOUT. Сохранены argv,
начальные CODE/VARS/disk, stderr и RunRecord; этот прогон не является replay.

Исходники [QEMU pflash_cfi01](https://github.com/qemu/qemu/blob/v9.2.4/hw/block/pflash_cfi01.c)
используют memory-backed flash и синхронные операции backend. Фильтр
[blkreplay](https://github.com/qemu/qemu/blob/v9.2.4/block/blkreplay.c)
приостанавливает coroutine до replay checkpoint. Наблюдение согласуется с
ожиданием checkpoint при предварительной загрузке pflash; это объяснение
механизма, а не заявление о доказанной поддержке любого flash workload.

Проверены две минимальные конфигурации с одинаковыми исходными файлами:

- `out/experiments/1790161542054467357-pflash-unfiltered/`: pflash без фильтра,
  disk с фильтром. Настоящие record и replay, оба exit 33, одинаковые 488 байт
  raw serial, SHA-256 `a6d3abde1afb7b0c4ed0d36095c2c63f2029de0eeac087a315cb7de91e70b967`.
- `out/experiments/1790161542033875155-rom/`: диагностический `-bios` с
  конкатенацией исходных VARS+CODE; оба exit 33 и одинаковые raw serial.
  Этот ROM-вариант не выбран основным профилем.

Основной harness сохраняет pflash и включает первый совместимый вариант.
Ранние эксперименты подтвердили совпадение bytes, но повторная проверка
upstream QEMU выявила потерю exit code. Поэтому окончательный профиль
дополнен патчем [ADR-0002](adr/0002-qemu-replay-exit.md); ранние traces
не совместимы с ним и не используются как доказательства финального профиля.
Record и replay начинают с одинаковых CODE, VARS и disk; результат record
не используется как исходный VARS replay. Дополнительно сравниваются полные
логические bytes итоговых VARS и disk после materialization overlays, помимо
обязательных raw serial и вердикта. Никакие строки serial не удаляются.

## Завершение и повторяемая сборка

В QEMU 9.2.4 `isa-debug-exit` запрашивает shutdown с кодом `(value<<1)|1`.
Флаг `-no-shutdown` исключён: он сохраняет остановленную VM и превращает
правильный guest PASS в TIMEOUT. Первоначальный такой отказ сохранён в
`out/runs/1790161139164622223-18109-pass-boot-test/`.

Upstream replay сохраняет cause shutdown без exit code; воспроизведение
может вернуть 0 вместо 33. Исправление сохраняет и восстанавливает код в
trace, включая последний shutdown при завершении записи. Harness не считает
0 успешным PASS. Проверяются запись/воспроизведение PASS (33) и FAIL (35).

UEFI release собирается без CodeView/PDB directory (`/debug:none`), поскольку
LLD PDB GUID различался для двух target directories. Kernel сохраняет DWARF
для `debug --run`; его source paths отображаются на `/nanox`. Точный PE
timestamp равен нулю. Бинарное сравнение выполняется после сборки, без
нормализации или исправления полученных EFI/ELF bytes.
