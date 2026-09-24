# M1: собственная загрузка и ядро

Документ фиксирует решения этапа M1 (ARCHITECTURE.md §15) и способы их
проверки. Стенд, harness и формат записи запусков — [m0-bench.md](m0-bench.md);
интерфейс boot info (версия 1.1), манифест и коды ошибок загрузчика —
[boot-info.md](boot-info.md).

Конвенции ниже (формат initramfs, раскладка GDT/IDT, адресное пространство,
формат отчёта об исключении, код выхода 41, источник таймера, допуски
проверок) в ARCHITECTURE.md не заданы; это решения M1.

## 1. Критерии M1 и их проверка

| Критерий (ARCHITECTURE.md §15) | Где реализовано | Чем проверено |
| --- | --- | --- |
| UEFI loader загружает ядро и initramfs | `boot/uefi/loader.c`, `kernel/initramfs.c`, `tools/image/mkinitrd.py` | сценарии `normal` (хеш initramfs из лога ядра равен хешу `out/initrd.img`), `missing-initrd`, `corrupt-initrd`, `bad-initrd`; host-тесты чтения cpio |
| Корректно передаёт карту памяти и завершает Boot Services | `boot/uefi/loader.c`, `boot/uefi/mmap_convert.c`, `kernel/bootinfo_check.c`, `kernel/main.c` (`reclaim_boot_memory`) | валидатор boot info в каждом сценарии; host-тесты перевода и проверки карты; в `normal` ядро переходит на свои таблицы страниц, возвращает allocator'у всю память boot services и стек загрузчика и затирает каждую такую страницу (`poisoned=`), после чего загрузка продолжается до TEST PASS |
| Ядро устанавливает свои таблицы, обработчики исключений и стек | `kernel/arch/x86_64/{gdt.c,idt.c,isr.S,entry.S}`, `kernel/mm/vmm.c` | маркеры `cpu gdt+tss+idt loaded`, `vmm ok`; сценарии `ud`, `gp`, `divzero`, `stackoverflow` (двойная ошибка на IST-стеке); самопроверка `#BP` с возвратом |
| Работают физический allocator, page tables, timer и panic report | `kernel/mm/pmm.c`, `kernel/mm/pt.c`, `kernel/mm/vmm.c`, `kernel/arch/x86_64/timer.c`, `kernel/panic.c` | самопроверки `selftest pmm`, `selftest vmm`, `timer ok`; сценарии `doublefree`, `timer-masked`, `panic`; host-тесты allocator'а и построителя таблиц |
| Преднамеренный page fault даёт проверяемую диагностику | `kernel/faults.c`, `kernel/panic.c`, `tools/bench/harness.py`, `tools/bench/elfsym.py` | сценарии `pagefault`, `nullderef`, `wprotect`, `nxexec`: harness сверяет вектор, код ошибки, CR2, функцию, в которой RIP (по таблице символов `kernel.elf`), и строку обхода таблиц страниц |

Готовность «повторяемая загрузка собственного ядра и предсказуемый аварийный
сценарий» проверяется `harness.py repeat normal pagefault --count 3` (часть
`make test`): три загрузки и три аварии дают одинаковые serial-маркеры,
включая адреса, регистры и обход стека; маскируются только измерения
времени (`ticks=`, `tsc_delta=`, `lapic_per_10ms=`).

## 2. Последовательность загрузки ядра

| Шаг | Маркер |
| --- | --- |
| `_start`: запомнить RSP загрузчика, перейти на свой стек | — |
| GDT, TSS, IDT | `NANOX: cpu gdt+tss+idt loaded boot_stack=... ist=3` |
| проверка boot info (identity mapping прошивки) | `NANOX: bootinfo ok version=1.1 ...` |
| RSP загрузчика = верх стека из boot info; сейчас работаем на своём стеке | panic при расхождении |
| физический allocator из `USABLE` | `NANOX: pmm ok span_pages=... managed=... free=...` |
| свои таблицы страниц, `CR3`, проверка прав | `NANOX: vmm ok root=... physmap=0xffff800000000000 ...` |
| возврат памяти boot services и стека загрузчика, затирание | `NANOX: pmm reclaimed boot_reclaimable=N loader_stack=16 pages poisoned=N+16 free=...` |
| проверка initramfs | `NANOX: initramfs ok ...`, `NANOX: initramfs release "..."` |
| самопроверки | `NANOX: selftest breakpoint ok`, `NANOX: selftest pmm ok`, `NANOX: selftest vmm ok` |
| таймер | `NANOX: timer ok source=lapic vector=0x40 hz=100 ...` |
| сценарий из `nanox.test=` | `NANOX: TEST PASS` и т. д. |

## 3. Стеки, GDT, TSS, IDT

Стеки лежат в образе ядра (секция `.stacks` без содержимого в файле,
`kernel/arch/x86_64/kernel.ld`); под каждым — страница-ограничитель, которая
никогда не отображается:

| Стек | Размер | Назначение |
| --- | ---: | --- |
| boot | 64 KiB | весь код ядра M1 |
| IST1 | 16 KiB | `#DF` |
| IST2 | 16 KiB | NMI |
| IST3 | 16 KiB | `#MC` |

GDT: `0x00` null, `0x08` код ядра (64-bit, DPL 0), `0x10` данные ядра,
`0x18` TSS (16 байт). Пользовательских сегментов нет до M2. TSS содержит
только IST-указатели; `RSP0` не используется, пока нет user mode.

IDT: 256 одинаковых заглушек по 16 байт (`isr.S`), все — interrupt gates DPL 0
(IF сбрасывается при входе). Для векторов без кода ошибки заглушка кладёт 0,
так что кадр `struct nx_trap_frame` одинаков для всех векторов.

| Вектор | Обработка |
| --- | --- |
| 0–31 кроме 3 | отчёт об исключении, выход 41 |
| 3 (`#BP`) | счётчик, возврат (проверяется самотестом) |
| 0x20–0x2F | линии 8259 после перенастройки; все замаскированы, приход — «unexpected interrupt», выход 41 |
| 0x40 | таймер local APIC |
| 0xFF | spurious local APIC: счётчик, без EOI |
| прочие | «unexpected interrupt», выход 41 |

## 4. Отчёт о panic и исключениях

Код выхода QEMU для необработанного исключения — **41** (значение `0x14` в
порт `0xF4`, `abi/nanox/diag.h`); класс отказа в harness — `exception`.
Формат:

```text
NANOX: EXCEPTION #PF vector=14 error=0x2 rip=0x00000000002024f3 cr2=0x00000000dead0000
NANOX: EXCEPTION #PF present=0 write=1 user=0 reserved=0 fetch=0
NANOX: EXCEPTION #PF mapping va=0x00000000dead0000 not-mapped
NANOX: REGS rip=... cs=0x0008 rflags=... rsp=... ss=0x0010
NANOX: REGS rax=... rbx=... rcx=... rdx=...
NANOX: REGS rsi=... rdi=... rbp=... r8=...
NANOX: REGS r9=... r10=... r11=... r12=...
NANOX: REGS r13=... r14=... r15=...
NANOX: REGS cr0=... cr2=... cr3=... cr4=... efer=...
NANOX: BACKTRACE 0 0x00000000002024f3
NANOX: BACKTRACE 1 0x0000000000200b93
...
NANOX: PANIC unhandled #PF at rip=0x00000000002024f3
```

- Для `#PF` печатаются разбор кода ошибки и результат обхода таблиц страниц
  ядра по адресу CR2 (`not-mapped` или `pa=... size=4K perms=r-x`).
- Если CR2 попадает в страницу-ограничитель стека, печатается
  `NANOX: EXCEPTION stack overflow: guard page of stack <имя> hit at <адрес>`.
- `BACKTRACE 0` — адрес сбоя, остальные — адреса возврата по цепочке RBP
  (ядро собирается с `-fno-omit-frame-pointer`). Обход останавливается на
  RBP = 0 (его обнуляет `_start`), на указателе вне стеков ядра или после 32
  кадров. Функции, вызванные хвостовым вызовом, в цепочке не видны.
- `nx_panic()` печатает `NANOX: PANIC <сообщение>`, затем `BACKTRACE` с
  кадра вызывающей функции и выходит с кодом 37.
- Повторная ошибка во время отчёта печатает `NANOX: PANIC nested ...` и
  сразу завершает работу.

Harness разбирает первую строку `EXCEPTION` и строки `BACKTRACE`,
символизирует адреса по `.symtab` файла `out/kernel.elf`
(`tools/bench/elfsym.py`, адреса возврата — по `адрес − 1`) и сохраняет
результат в `record.json` (поля `exception`, `backtrace`). Ожидания
сценария могут требовать мнемонику, вектор, код ошибки, CR2, функцию RIP,
равенство CR2 и RIP (ошибка выборки команды) и функции в backtrace.

## 5. Физический allocator

`kernel/mm/pmm.c`: две битовые карты по одному биту на страницу 4 KiB в
диапазоне `[0, конец самого верхнего региона, который может перейти
allocator'у)`: `managed` (страница принадлежит allocator'у) и `used`.

- Allocator'у передаются только страницы не ниже 1 MiB.
- При старте — все `USABLE`. После перехода на свои таблицы — `BOOT_RECLAIMABLE`
  (код и данные boot services и загрузчика) и `KERNEL_STACK` (стек
  загрузчика). `INITRD`, `ACPI_*`, `BOOT_INFO`, образ ядра и runtime-регионы
  прошивки не передаются.
- Сами битовые карты занимают первые подходящие страницы `USABLE` выше
  1 MiB и навсегда остаются вне allocator'а.
- Выделение — next-fit по битовой карте. Ошибки освобождения:
  `E_ALIGN`, `E_UNMANAGED` (страница не allocator'а), `E_DOUBLE_FREE`. Ядро
  (`nx_page_free`) превращает любую из них в panic.

## 6. Адресное пространство ядра

Свои четырёхуровневые таблицы (`kernel/mm/pt.c` строит, `kernel/mm/vmm.c`
раскладывает). Все отображения ядра — supervisor-only и global; включены
`EFER.NXE` (без поддержки NX ядро останавливается panic), `CR0.WP`, `CR4.PGE`.

| Виртуальный адрес | Содержимое | Права |
| --- | --- | --- |
| `0x0` и вся нижняя половина, кроме образа ядра | не отображено | — |
| `0x200000` (= физический адрес) | `.text` | r-x |
| далее | `.rodata` | r-- |
| далее | `.data`, `.bss` | rw- |
| далее | стеки; страницы-ограничители не отображены | rw- |
| `0xFFFF800000000000 + phys` | physmap: все RAM-регионы, кроме образа ядра (`USABLE`, `BOOT_RECLAIMABLE`, `KERNEL_STACK`, `BOOT_INFO`, `ACPI_*`, `FIRMWARE_RUNTIME`, `INITRD`); 2 MiB страницы, где позволяет выравнивание | rw- |
| `0xFFFF800000000000 + phys` для MMIO | по запросу (`nx_vmm_map_mmio`), 4 KiB, PCD+PWT | rw- |
| `0xFFFFC00000000000` | страница самопроверки VMM | rw- |

Ядро остаётся слинкованным по физическому адресу в нижней половине: перенос
в верхнюю половину потребовал бы от загрузчика строить таблицы страниц, а
M1 требует, чтобы таблицы строило ядро. Решение пересматривается до
появления пользовательских адресных пространств (M2).

После переключения `CR3` ядро проверяет установленные права: `.text` без W и
NX, `.rodata` без W с NX, `.data` с W и NX, четыре страницы-ограничителя, нулевая
страница, адрес `0xDEAD0000` и псевдоним образа ядра в physmap не отображены.
Страницы таблиц не освобождаются (снятие отображения очищает только лист).

## 7. Таймер

- 8259 перенастроены на векторы `0x20–0x2F` и полностью замаскированы.
- Local APIC (xAPIC, MMIO по адресу из `IA32_APIC_BASE`) включается через
  SVR, spurious-вектор `0xFF`.
- Таймер APIC, делитель 16, калибруется по каналу 2 PIT (1 193 182 Hz,
  режим 0, опрос бита OUT2 порта `0x61`, без прерываний): число отсчётов APIC
  за один период PIT в 10 мс (11 932 отсчёта PIT).
- Затем таймер работает в периодическом режиме на 100 Hz (вектор `0x40`),
  ядро включает прерывания и отмеряет по PIT окно 30 × 10 мс. Проверка
  проходит, если пришло от 20 до 40 прерываний (±1/3 от 30). Оба источника в
  QEMU тактуются одними виртуальными часами; при подготовке M1 в 32
  запусках (из них 12 — при четырёх одновременно работающих QEMU) приходило
  29–30 прерываний.
- После проверки таймер маскируется и прерывания запрещаются: до
  планировщика (M2) периодические прерывания не нужны.
- Все ожидания ограничены числом опросов порта; сбой PIT или отсутствие
  прерываний дают `NANOX: TEST FAIL timer: <причина>`, а не зависание.

## 8. initramfs

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
заканчиваться `TRAILER!!!` (после него допускаются нули). Коды:
`E_TRUNCATED`, `E_MAGIC`, `E_HEX`, `E_NAME`, `E_BOUNDS`, `E_TYPE`,
`E_NO_TRAILER`, `E_NOT_FOUND`.

## Сценарии стенда

Все запускаются `make test` вместе со сценариями M0
([m0-bench.md §6](m0-bench.md#6-headless-harness)).

| Сценарий | `nanox.test=` / образ | Ожидание (статус QEMU) |
| --- | --- | --- |
| `normal` | — | PASS (33); маркеры шагов раздела 2; хеши ядра и initramfs равны хешам на хосте; `ram_mapped` 250–262 MiB |
| `missing-initrd` | нет `INITRD.IMG` | `loader_error` `E_INITRD_OPEN` (39) |
| `corrupt-initrd` | байт initramfs инвертирован | `loader_error` `E_INITRD_HASH` (39) |
| `bad-initrd` | 150 байт текста вместо архива, манифест им соответствует | `panic` `initramfs invalid: E_MAGIC at offset 0` (37) |
| `ud` | `ud` | `exception` `#UD`, вектор 6, RIP в `nx_fault_ud` (41) |
| `gp` | `gp` | `exception` `#GP`, вектор 13, RIP в `nx_fault_gp` (41) |
| `divzero` | `divzero` | `exception` `#DE`, вектор 0, RIP в `nx_fault_divzero` (41) |
| `pagefault` | `pagefault` | `exception` `#PF`, error `0x2`, CR2 `0xdead0000`, RIP в `nx_fault_pagefault`, `mapping ... not-mapped` (41) |
| `nullderef` | `nullderef` | `#PF`, error `0x0`, CR2 0, RIP в `nx_fault_nullderef` (41) |
| `wprotect` | `wprotect` | `#PF`, error `0x3` (запись в присутствующую страницу), страница `.rodata` с правами `r--` (41) |
| `nxexec` | `nxexec` | `#PF`, error `0x11` (выборка команды), CR2 = RIP, страница данных `rw-` (41) |
| `stackoverflow` | `stackoverflow` | `#DF` (вектор 8) на IST-стеке, строка `stack overflow: guard page of stack boot hit` (41) |
| `doublefree` | `doublefree` | `panic` `pmm: free of page ... rejected: E_DOUBLE_FREE`, в backtrace `nx_page_free` и `kernel_main` (37) |
| `timer-masked` | `timer-masked` | `test_fail` `timer: no timer interrupts in the window (ticks=0 expected=30)` (35) |

Дополнительно `make test` выполняет `harness.py repeat normal pagefault
--count 3` (раздел 1); результат — `out/runs/<время>-repeat-<сценарий>/repeat.json`.

## Что проверено при подготовке M1

Эталонная среда та же, что в M0 (Ubuntu 24.04, QEMU 8.2.2, TCG, без KVM).
Выполнено в рабочем дереве и в свежем `git clone`:

- `make doctor && make && make test` — код выхода 0: host-тесты C
  (340 проверок, в том числе 20 000 случайных искажений boot info и 20 000 —
  initramfs; UBSan в режиме trap), 29 Python-тестов, 21 сценарий QEMU с
  ожидаемыми вердиктами, повторяемость `normal` и `pagefault` (3 + 3
  запуска, маркеры совпали);
- host-тесты дополнительно собраны gcc с `-fsanitize=address,undefined`;
- в host-тесты по одной вносились ошибки в allocator, построитель таблиц,
  чтение initramfs и валидатор — каждую тесты обнаружили;
- `make repro-check` — `BOOTX64.EFI`, `kernel.elf`, `initrd.img`,
  `nanox.img` побайтово совпали в трёх сборках;
- `make debug-check` — GDB остановился в `kernel_main` на собственном стеке
  ядра.

## Что не проверено и ограничения

- Повторная попытка `ExitBootServices` (ветка при изменившемся map key) на
  стенде не возникает и не проверена; она реализована по UEFI 2.11 §7.4.
- При `#DF` сохранённые CS:RIP по SDM не определены; QEMU сообщает RIP
  команды, вызвавшей исходную ошибку, и сценарий `stackoverflow` на это
  опирается. На другом эмуляторе или железе это может отличаться.
- NMI и `#MC` имеют IST-стеки, но их доставка не проверялась.
- Один CPU; SMP, x2APIC, HPET, ACPI-таблицы не используются.
- physmap даёт ядру запись во всю RAM, кроме образа ядра; изоляция внутри
  ядра не заявляется.
- initramfs после проверки остаётся в памяти (регион `INITRD` не
  освобождается).
- Все результаты получены в QEMU (TCG) и не доказывают работу на реальном
  оборудовании.
