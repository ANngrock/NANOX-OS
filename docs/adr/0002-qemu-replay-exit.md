# ADR-0002: сохранение статуса shutdown в QEMU replay

Дата: 2026-09-23. Статус: принято для ограниченного профиля M0.

## Основание

Обычный boot и запись QEMU 9.2.4 завершаются кодом 33. Replay той же записи
может завершиться кодом 0 при одинаковых raw serial, итоговых VARS и disk.
Неуспешная проверка сохранена в
`out/runs/1790161777762347455-26670-pass-replay-play/`.

В upstream [replay.c](https://github.com/qemu/qemu/blob/v9.2.4/replay/replay.c)
shutdown event содержит только cause. При воспроизведении вызывается
`qemu_system_shutdown_request`, тогда как устройство `isa-debug-exit` вызывает
вариант `with_code`. [runstate.c](https://github.com/qemu/qemu/blob/v9.2.4/system/runstate.c)
хранит код отдельно. Поэтому требуемые 33/35 теряются при воспроизведении
самого shutdown event.

## Решение

Закреплённый QEMU 9.2.4 собирается Nix с локальным
`tools/qemu/replay-exit-code.patch`. Shutdown event получает 32-bit exit code;
play восстанавливает его через `with_code`. Заключительный shutdown event
сохраняет последний записанный код, чтобы не затереть его нулём.
Версия trace изменена с `0xe0200c` на `0xe0200d`, package version содержит
`nanox-replay-exit-v1`. Старые traces несовместимы и не переиспользуются.

Собирается только x86_64 system target и нужные host tools; интерфейс гостя
и выбранная машина `pc-q35-9.2` не изменяются. Патч входит в source manifest,
а бинарный hash QEMU — в RunRecord и replay preflight.

## Проверка и ограничения

Harness сохраняет строгую проверку markers + exit 33/35. Автоматическая
матрица записывает и воспроизводит оба сценария: PASS и намеренный FAIL.
Сравниваются raw serial, guest verdict, итоговые VARS и disk. Актуальные
результаты — в STATUS; этот ADR сам по себе не доказывает прохождение теста.

Это локальный downstream patch, не заявление об исправлении upstream.
Первый вход в Nix требует сборки QEMU из исходников. Replay по-прежнему
ограничен профилем M0; произвольные устройства и reverse debugging не обещаны.
