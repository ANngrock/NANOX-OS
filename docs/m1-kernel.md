# M1: собственная загрузка и ядро

Документ фиксирует решения этапа M1 (ARCHITECTURE.md §15) и способы их
проверки. Стенд, harness и формат записи запусков — [m0-bench.md](m0-bench.md);
интерфейс boot info и правила ошибок — [boot-info.md](boot-info.md).

## Критерии M1

| Критерий (ARCHITECTURE.md §15) | Где реализовано | Чем проверено |
| --- | --- | --- |
| UEFI loader загружает ядро и initramfs | `boot/uefi/loader.c`, `kernel/initramfs.c` | сценарии `normal`, `missing-initrd`, `corrupt-initrd`, `bad-initrd`; host-тесты initramfs |

## initramfs

Формат — cpio «newc» (magic `070701`, заголовок 110 ASCII-байт, 13
шестнадцатеричных полей по 8 символов, имя и данные выровнены на 4 байта от
начала архива, конец — запись `TRAILER!!!`). Выбран как простой
документированный формат, который читают стандартные инструменты (GNU cpio
2.15 прочитал архив сборки при подготовке M1), а пишется детерминированно.

Сборка: `tools/image/mkinitrd.py` упаковывает каталог `initrd/` репозитория в
`out/initrd.img`. Все поля заголовков выводятся только из содержимого:
записи отсортированы по пути, номера inode последовательные, uid/gid 0,
режимы фиксированы (каталоги `040755`, файлы `0100644`), mtime —
`SOURCE_DATE_EPOCH` или 0. Допускаются только обычные файлы и каталоги.

Путь до ядра:

1. `mkimage.py` кладёт архив в `\NANOX\INITRD.IMG` и записывает его размер и
   SHA-256 в манифест версии 2.
2. Загрузчик проверяет размер и хеш (`E_INITRD_OPEN`, `E_INITRD_SIZE`,
   `E_INITRD_HASH`), копирует архив в страницы типа `INITRD` и передаёт адрес,
   размер и хеш в boot info 1.1.
3. Ядро заново считает SHA-256 полученных байт и сравнивает с boot info,
   проходит весь архив (`nx_cpio_validate`) и ищет обязательный файл
   `etc/nanox/release`. Любое расхождение — panic
   `initramfs invalid: <код> at offset <смещение>`.

Правила чтения (`kernel/initramfs.c`): только magic `070701`; поля — ровно 8
шестнадцатеричных цифр; `namesize` от 2 до 4096, имя завершено NUL на
позиции `namesize-1`, без NUL внутри и без ведущего `/`; данные не выходят за
конец архива; тип записи — обычный файл или каталог; архив обязан
заканчиваться `TRAILER!!!` (после него допускаются нули).

Коды: `E_TRUNCATED`, `E_MAGIC`, `E_HEX`, `E_NAME`, `E_BOUNDS`, `E_TYPE`,
`E_NO_TRAILER`, `E_NOT_FOUND`.

Маркеры: `NANOX: loader initrd sha256 ok <hex> at <адрес>`,
`NANOX: initramfs ok entries=N bytes=M sha256=<hex>`,
`NANOX: initramfs release "<первая строка etc/nanox/release>"`.

## Сценарии стенда

| Сценарий | Образ | Ожидание |
| --- | --- | --- |
| `normal` | как в M0 | PASS; SHA-256 initramfs из лога ядра совпадает с хешем `out/initrd.img` на хосте; строка `release` совпадает с `initrd/etc/nanox/release` |
| `missing-initrd` | без `INITRD.IMG` | FAIL `loader_error` `E_INITRD_OPEN`, статус 39 |
| `corrupt-initrd` | один байт initramfs инвертирован после записи манифеста | FAIL `loader_error` `E_INITRD_HASH`, статус 39 |
| `bad-initrd` | 150 байт текста вместо архива, манифест им соответствует | FAIL `panic`, `NANOX: PANIC initramfs invalid: E_MAGIC at offset 0`, статус 37 |
