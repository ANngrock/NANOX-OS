# Стенд, воспроизводимость и результаты

Статус: контракт для реализации M0 и дальнейших расширений.

## 1. Три разные проверки

1. **Повторяемая сборка:** одинаковые исходники/toolchain/config дают одинаковые kernel/loader bytes.
2. **Повторный boot:** несколько новых запусков корректно проходят сценарий; их raw logs не обязаны совпадать при разных входах.
3. **Record/replay:** воспроизведение конкретной записи с тем же исходным состоянием и входами даёт те же наблюдаемые результаты.

Побайтное равенство serial в третьем случае — полезный тест этого канала. Оно не доказывает равенство всего состояния памяти и не означает универсальную обратимость.

## 2. Профили QEMU

| Профиль | Использование |
| --- | --- |
| boot-test | Headless, фиксированная машина, TCG, 1 vCPU, COM1 log, timeout, test exit |
| replay-record | Тот же guest, корректно настроенный QEMU record backend |
| replay-play | Те же входные hashes/initial state, потребление записанного trace |
| debug | GDB stub, symbols, возможность остановки на entry; без автоматического timeout теста |

Закрепить точное versioned machine name, CPU features, RAM, QEMU build/version, OVMF CODE/VARS hashes и список устройств. Имена `q35`, `host`, `max` без конкретного закрепления не служат идентичностью эксперимента.

В baseline использовать TCG single-thread, один vCPU, фиксированное начало RTC, отключённую сеть и отсутствие внешнего live input. RDRAND/RDSEED и необязательные источники случайности не использовать в M0. Настройка тестового seed не переносится в production CSPRNG.

QEMU replay использует `icount`; для блочных и сетевых путей нужны соответствующие replay backends/filters. Конкретные аргументы формирует и проверяет harness на закреплённой версии, а не копирует случайный пример. Руководство [QEMU record/replay](https://www.qemu.org/docs/master/system/replay.html) также описывает serial и reverse debugging.

Firmware pflash CODE/VARS входит в профиль явно. Нужно испытать выбранный способ подключения pflash с replay на фактической версии QEMU. Нельзя объявить все block devices покрытыми, проверив только основной disk image.

## 3. Исходное состояние

- OVMF CODE неизменяемый.
- VARS для record начинается с исходного шаблона, для replay — с той же исходной версии, не с результата record.
- Базовый ESP/диск неизменяемый; writable layers отдельные.
- Точные QEMU argv записываются массивом строк, не неоднозначной shell-строкой.
- Replay trace связывается с source/artifact/profile hashes.
- При несовпадении входов harness отвергает replay, а не пытается продолжить с новым kernel.
- Live model/network replies не запрашиваются повторно в replay-сценарии.

Использовать raw `serial.bin` для побайтного сравнения. Человекочитаемый `serial.txt` может быть отдельным представлением, но не источником вердикта равенства.

Host timestamps, run IDs и paths хранятся в RunRecord, не внедряются в guest serial как новые данные при воспроизведении.

## 4. Автоматический вердикт

Для test profile добавить `isa-debug-exit`, iobase `0xf4`, iosize 4. Guest пишет 32-bit значение: PASS = `0x10`, FAIL = `0x11`. Ожидаемый QEMU status по механизму устройства: `(value << 1) | 1`, соответственно 33 и 35. Harness преобразует ожидаемый 33 в свой exit 0, а не считает любой ненулевой QEMU exit неуспехом.

Обязательно проверить фактическую семантику закреплённого QEMU этим тестом. Код 0 от неожиданно закрытого QEMU без marker/status не считается PASS. Ошибка запуска QEMU, сигнал завершения, timeout и guest failure имеют разные verdict reasons.

Успех требует одновременно:

- ожидаемых loader/kernel markers;
- отсутствия PANIC/FAIL marker;
- финального `NANOX:TEST:PASS`;
- ожидаемого статуса завершения;
- отсутствия timeout/harness error.

У boot-test есть внешний лимит времени, по умолчанию 30 секунд после старта процесса QEMU. Сборка имеет отдельный лимит и не расходует boot timeout. Медленный стенд может явно задать другой timeout, который сохраняется в profile/RunRecord.

Harness завершает только порождённый им процесс/его группу. Не убивать все процессы QEMU в системе. Debug mode не слушает public network; предпочтителен локальный socket либо loopback endpoint.

## 5. RunRecord v1

Каталог `out/runs/<run-id>/`:

```text
record.json
serial.bin
serial.txt
qemu-stderr.txt
build.log
replay.bin                 # только для record/replay
inputs.json
```

Обязательные поля record:

| Группа | Поля |
| --- | --- |
| Schema | `schema_version`, `run_id`, `scenario`, `mode` |
| Source | commit, dirty flag, source manifest/patch hash |
| Tools | Rust version, target/profile, flake.lock hash, Cargo.lock hash, QEMU version/build |
| Machine | versioned machine, CPU/features, vCPU, RAM, firmware hashes |
| Inputs | ELF/EFI/image hashes, initial VARS hash, scenario input hash |
| Invocation | argv, relevant environment, timeout |
| Outputs | serial hash, stderr/log paths, replay trace hash when present |
| Result | QEMU raw exit, harness verdict/reason, elapsed duration |

Пути к API credentials и тем более значения секретов не копируются в отчёт. Для dirty checkout одного commit недостаточно: manifest охватывает все build inputs, включая незакоммиченные файлы.

RunRecord — отчёт испытания. `Generation` — согласованный устанавливаемый набор компонентов M6. Эти сущности связаны, но не тождественны.

## 6. Матрица M0

| Сценарий | Ожидание |
| --- | --- |
| Корректный образ | BootInfo validated, PASS, expected exit |
| Отсутствует kernel | Loader error, kernel entry не достигнут |
| Усечённый/повреждённый ELF | Reject до handoff |
| Некорректные сегменты | Reject, отсутствие частичного запуска |
| Тестовая ошибка ядра | FAIL и соответствующий exit |
| Намеренное зависание | TIMEOUT, завершён только child QEMU |
| Две чистые сборки | Совпадают EFI/ELF hashes |
| Replay recorded boot | Равны raw serial и guest verdict |
| Replay с другим ELF hash | Harness отвергает несовместимые inputs |

Host fixtures для ELF/BootInfo проверяют реальные границы формата. Интеграционный QEMU-test доказывает переход между компонентами. Наличие одних unit tests недостаточно.

## 7. Воспроизводимость артефактов

Зафиксировать debug path mapping, timestamp-sensitive PE/linker fields, build IDs, locale/timezone и входные manifests там, где они влияют на bytes. Не добавлять build date автоматически в guest binary.

В M0 обязательны одинаковые ELF/EFI при двух чистых сборках. FAT/GPT образ также стремиться сделать byte-identical через фиксированные volume IDs, GUIDs и timestamps; если это ещё не выполнено, отражать отдельно. Нельзя переносить успех binary check на образ целиком.

Хеширование и provenance не доказывают, что компилятор корректен или исходники безопасны. Они позволяют точно повторить и идентифицировать эксперимент.

## 8. Развитие replay

В M1–M2 выделяются `Clock`, `EntropySource`, IRQ dispatch и I/O completion interfaces. Это границы наблюдения и подмены в тестах. Реальная DMA-запись остаётся аппаратным событием с отдельной моделью.

В M3 записываются кадры bridge и ответы модели. В M4 — storage fault schedules. Позже SMP/GPU/реальные устройства включаются только вместе с определением поддержанного replay scope.

GDB reverse debugging требует пригодной replay/snapshot конфигурации. Наличие gdbstub само по себе не создаёт обратное исполнение. Если нужен лишь обычный breakpoint debug, его можно предоставить раньше reverse debugging.
