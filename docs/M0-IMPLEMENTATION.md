# Реализация M0

Состояние фактических проверок и ссылки на результаты: [STATUS](STATUS.md).
Среда и основной checkout: [ENVIRONMENT](ENVIRONMENT.md).

`boot/uefi` использует собственное подмножество UEFI 2.11: SystemTable,
BootServices, LoadedImage, SimpleFileSystem, File, MemoryDescriptor. Layout
используемых полей проверяется при компиляции. Номер загрузочного диска не
угадывается: volume берётся из LoadedImage текущего приложения. Источники:
[таблицы UEFI](https://uefi.org/specs/UEFI/2.11/04_EFI_System_Table.html) и
[протоколы файлов](https://uefi.org/specs/UEFI/2.11/13_Protocols_Media_Access.html).

ELF сначала целиком проверяется безопасным parser в `boot-protocol`, затем
loader выделяет отдельные физические страницы, обнуляет их и копирует segment
payload. `p_paddr` не задаёт физическое размещение. Поддержан сознательно узкий
статический ELF64 subset из BOOT-M0; неизвестные program header types и
relocation sections дают явную ошибку. Host fixture не является boot-ядром.

Arena фактически занимает 256 KiB внутри разрешённого ABI-окна 16 MiB. Header
находится по offset 0, reservations 4096, segments 8192, memory map 16384.
Loader и kernel используют общий предел реально отображённой памяти перед
созданием slice. Все необходимые allocations перечислены в reservations.
Финальная карта обходится по stride firmware; буфер выделен заранее.
ExitBootServices имеет максимум четыре попытки. С первого exit attempt
разрешены только получение новой карты и повтор выхода.

Новые четырёхуровневые page tables содержат kernel segments по flags,
RW/NX arena и stack с guard page, отдельные identity mappings trampoline,
GDT/IDT и переходного stack. Нет общего physical direct map. Trampoline
загружает собственные GDT/IDT, включает EFER.NXE и CR0.WP, меняет CR3/RSP и
переходит на ELF entry с BootInfo в RDI. Fatal-only обработчик ранних исключений
печатает диагностику; IST, полноценные исключения и IRQ относятся к M1.

Kernel проверяет header, отдельные buffers, reservations, сегменты и активную
CR3; читает реальные `.data` и BSS. Валидация не является memory allocator или
моделью полномочий Cognitive Core: M1/M2 ещё не реализованы.

## Вход тестового сценария

`NANOX/BOOT.CFG` — ровно 24 байта: `NXCFG001`, little-endian `u64 flags`
(0 обычный, 1 test), little-endian `u64 epoch`. Отсутствующий файл означает
обычный профиль. Неизвестные bits, длина или magic отвергаются.

Только test profile интерпретирует epoch `u64::MAX` как FAIL,
`u64::MAX-1` как hang и `u64::MAX-2` как panic. В обычном профиле эти числа
не инъецируют ошибки. Обычное ядро печатает IDLE и остаётся в `cli; hlt`.
`cargo xtask run` выполняет профиль `boot-test`, ожидая debug exit 33.
Отсутствие test flag запрещает доступ к test-only порту 0xf4.

## Артефакты и повторение

`out/nanox.img` — GPT-диск размером 96 MiB с FAT32 ESP; это файл.
Бинарники: `out/BOOTX64.EFI`, `out/KERNEL.ELF`. Сценарии создают отдельные
initial inputs и writable qcow2 overlays; firmware CODE не изменяется.
`out/runs/<id>/record.json` хранит argv, hashes, версии, raw exit, timeout и
вердикт. Raw serial никогда не очищается для сравнения replay.

Fingerprint исходников включает Cargo/flake/toolchain, `.cargo`, `boot`,
`kernel`, `crates`, `tools`, `tests` и machine profile, в том числе untracked
файлы. Документация статуса не влияет на бинарники и исключена из fingerprint;
commit, dirty flag и hash полного Git diff записываются отдельно.

`cargo xtask reproduce-build` создаёт два новых target-каталога, сравнивает
EFI/ELF и сохраняет report. Linker PE timestamp равен 0, ELF build-id отключён,
пути debug information отображаются на `/nanox`. Эта проверка не выдаётся
за replay или за проверку равенства всего disk image.

`cargo xtask test --replay` использует настоящий QEMU `icount` record/replay,
сохраняя trace. Профиль опирается на
[QEMU replay documentation](https://www.qemu.org/docs/master/system/replay.html).
Результат считается доказанным только при равенстве raw serial и вердиктов.
Replay с изменённым ELF проверяется на отдельной повреждённой копии input и
отвергается до запуска QEMU. `cargo xtask debug --run <id>` — обычный GDB stub
на loopback, с `-S`. В GDB: `file out/runs/<debug-id>/KERNEL.ELF`,
`target remote 127.0.0.1:1234`, `set breakpoint auto-hw off`,
`break kernel_main`, `continue`. Программная точка останова проверена в QEMU;
аппаратная `hbreak` в этом профиле не поддерживается. Это не обещание reverse
debugging.
