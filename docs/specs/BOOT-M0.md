# Boot M0: контракт loader и ядра

Статус: спецификация первой реализации. Никакие приведённые структуры, адреса и команды пока не проверены запуском; это требования к реализации и тестам. Изменение layout требует обновления версии и проверок обеих сторон.

## 1. Платформа и образ

- x86-64, QEMU versioned q35, 1 vCPU, 512 MiB RAM, TCG.
- OVMF CODE закреплён хешем, VARS копируется из одного исходного шаблона в каталог каждого запуска.
- Secure Boot в M0 выключен; цепочка подписей появится отдельным этапом.
- Загрузочный образ — файл с GPT/ESP/FAT, создаваемый host tools. Никаких физических дисков.
- ESP содержит `EFI/BOOT/BOOTX64.EFI` и `NANOX/KERNEL.ELF`.
- Loader находит свой boot volume через Loaded Image и Simple File System, а не через номер случайного диска.
- COM1: legacy UART на `0x3f8`, polling, без IRQ; stdout host не смешивается с guest serial.
- Guest network в M0 выключена. COM2 добавляется в M3.

Firmware читает boot-диск до handoff. Runtime block driver NANOX в M0 не нужен. Конкретный контроллер boot-диска фиксируется machine profile вместе с проверенным replay backend.

## 2. Сборка

Loader: `x86_64-unknown-uefi`, `#![no_std]`, `#![no_main]`, `extern "efiapi"` на firmware entry/calls, abort panic. Kernel: `x86_64-unknown-none`, статический ELF64, свой linker script, entry через ASM.

Нельзя выставлять один набор linker flags глобально сразу для host xtask, UEFI и kernel. Каждая команда Cargo указывает package/target/profile. `.cargo/config.toml` задаёт alias `xtask` и соответствующие target-specific настройки.

В M0 guest не использует heap `alloc`. Loader получает нужные страницы через firmware; после handoff все необходимые структуры уже размещены. Разрешённые compiler runtime/sysroot dependencies перечисляются в build report.

UEFI bindings включают полные layout используемых таблиц либо корректные offsets, GUID, status, pointer widths и calling conventions. Пропущенное поле в firmware-структуре нельзя заменить догадкой о расположении следующего. Поддерживаемое подмножество интерфейсов документируется.

## 3. ELF loader

Принимать ELF64 little-endian, `EM_X86_64`, `ET_EXEC`, поддержанную версию ELF. Dynamic linking, interpreter, relocations и PIE в M0 не поддерживать. Неподдержанное — явная ошибка.

До загрузки проверить:

- полный ELF header, размеры и расположение program header table;
- все сложения/умножения offsets без overflow;
- `filesz <= memsz`, попадание file ranges внутрь файла;
- допустимые alignment и согласованность offsets/virtual addresses;
- канонические virtual ranges и отсутствие пересечения зарезервированных окон;
- непересекающиеся page-aligned `PT_LOAD` ranges для поддерживаемого linker layout;
- отсутствие W+X сегментов;
- entry внутри executable load segment;
- верхние лимиты: файл 32 MiB, суммарные load regions 128 MiB, не более 32 program headers.

Это выбранные лимиты первой версии, а не аппаратные максимумы. `p_paddr` не используется как произвольное требование выделить firmware-memory: физическое размещение выбирает loader через firmware allocation, mapping сохраняется отдельно.

Каждый load segment копируется в выделенную память; BSS и хвост до конца выделенной страницы обнуляются. После ошибки переход в entry не выполняется. Тесты включают truncated header, overflow offset, overlap, bad entry, filesz > memsz и W+X.

## 4. Виртуальный layout первой версии

| Область | Адрес/правило |
| --- | --- |
| Kernel base | `0xffffffff80000000` |
| Kernel window | 128 MiB от kernel base |
| Handoff arena | `0xffffffff90000000`, максимум 16 MiB |
| Bootstrap stack top | `0xffffffff92000000`, 64 KiB вниз |
| Stack guard | Одна немаппированная страница ниже stack |
| Transition code/stack | Временное identity mapping выделенных физических страниц |
| Page size | 4 KiB; четырёхуровневые таблицы |

ASLR и general physical direct map не входят в M0. Физические allocations не обязаны совпадать с virtual addresses.

Loader строит минимальные новые page tables: kernel по segment flags, handoff arena как RW/NX для начальной передачи, stack RW/NX, transition code executable. Пользовательские mappings отсутствуют. Перед использованием NX проверяются поддержка CPU и правильная настройка соответствующего enable bit; недоступная требуемая возможность даёт диагностическую ошибку.

Непосредственно перед сменой CR3 выполняется ограниченный ASM trampoline без вызовов firmware/heap. Временный код, стек и необходимые descriptor tables остаются отображёнными. Переход на новые CR3/RSP/RIP должен быть явным; нельзя рассчитывать, что код загрузчика останется доступен после удаления firmware mappings.

M0 обязан иметь достаточную раннюю диагностику перехода, не объявляя полноценную подсистему исключений M1 реализованной. Если для корректного handoff требуются минимальные собственные GDT/IDT, они входят в этот переход; M1 расширяет их до штатных обработчиков и IRQ.

## 5. BootInfo v1

Общий crate `boot-protocol`, `repr(C)`, natural alignment 8. Все целочисленные поля фиксированного размера; в handoff нет Rust references, enums с нефиксированным layout и heap containers.

| Offset | Поле | Тип |
| --- | --- | --- |
| 0 | magic, байты `NXBOOT01` | `[u8; 8]` |
| 8 | major | `u16`, значение 1 |
| 10 | minor | `u16`, значение 0 |
| 12 | header_size | `u32`, значение 160 |
| 16 | total_size | `u64`, в v1 равно 160 |
| 24 | flags | `u64` |
| 32 | memory_map_phys | `u64` |
| 40 | memory_map_virt | `u64` |
| 48 | memory_map_len | `u64`, байты |
| 56 | memory_descriptor_size | `u32` |
| 60 | memory_descriptor_version | `u32` |
| 64 | reserved_ranges_phys | `u64` |
| 72 | reserved_ranges_virt | `u64` |
| 80 | reserved_ranges_count | `u32` |
| 84 | reserved_ranges_stride | `u32`, v1 = 24 |
| 88 | rsdp_phys | `u64`, 0 если отсутствует |
| 96 | kernel_entry_virt | `u64` |
| 104 | load_segments_phys | `u64` |
| 112 | load_segments_virt | `u64` |
| 120 | load_segments_count | `u32` |
| 124 | load_segments_stride | `u32`, v1 = 32 |
| 128 | pml4_phys | `u64` |
| 136 | stack_top_virt | `u64` |
| 144 | serial_io_port | `u16` |
| 146 | reserved0, нули | `[u8; 6]` |
| 152 | boot_epoch | `u64` |

`total_size` относится только к header/его будущим inline extensions, а не к отдельно указанным buffers. Все virtual pointers на map/ranges/segments лежат в handoff arena и доступны при входе. Физические адреса нужны для ownership/reclaim, но не являются готовыми Rust pointers.

`ReservedRange`: `phys_start:u64`, `page_count:u64`, `kind:u32`, `reserved:u32`; 24 байта. Kinds: 1 kernel, 2 boot-info buffers, 3 page tables, 4 bootstrap stack, 5 transition/descriptor tables, 6 initramfs (зарезервировано для расширения).

`LoadedSegment`: `phys_start:u64`, `virt_start:u64`, `memory_size:u64`, `flags:u32`, `reserved:u32`; 32 байта. Flags соответствуют ELF PF_R/PF_W/PF_X; неизвестные биты отвергаются.

Reserved ranges описывают все выделения, которые ядро не вправе автоматически вернуть allocator. Освобождение transition areas возможно только после полного переключения на собственные механизмы M1.

UEFI memory map обходится по `memory_descriptor_size`, возвращённому firmware; нельзя считать stride равным размеру своей сокращённой структуры. Все проверки count × stride и границ обязательны.

В v1 flags: bit 0 — успешный ExitBootServices, bit 1 — test profile. Другие биты и reserved поля равны нулю. Boot epoch в тестовом профиле задаётся входным manifest и не является секретом; стабильный test epoch не используется как криптографическая случайность.

Size/offset assertions проверяются на обоих targets; host golden fixture проверяет тот же layout.

## 6. Последовательность handoff

1. Проверить firmware entry, инициализировать serial, открыть boot volume.
2. Прочитать и проверить ELF, выделить и заполнить сегменты.
3. Подготовить stack, BootInfo arena, page tables и transition allocations.
4. Зарезервировать достаточно места для последней memory map и служебных списков.
5. Получить актуальную memory map и её key после всех обычных allocations.
6. Выполнить `ExitBootServices` с актуальным key. При устаревшем key получить карту заново по предусмотренному retry path; не продолжать обычные filesystem/alloc операции после начала выхода.
7. После успеха использовать только подготовленные ресурсы и прямые аппаратные механизмы.
8. Заполнить окончательные handoff flags/lengths в собственной памяти, выключить maskable interrupts, очистить DF, переключить CR3 и stack, войти в kernel ASM entry.
9. Kernel entry принимает `RDI = BootInfo virtual pointer`, устанавливает корректное выравнивание stack перед вызовом Rust `extern "C" kernel_main`, не возвращается в firmware.

Проверять не только результат `ExitBootServices`, но и отсутствие более поздних обращений к Boot Services. RSP на границе вызова Rust должен соответствовать System V ABI; нельзя считать UEFI calling convention автоматически совместимым с ним.

## 7. Что делает первое ядро

- Инициализирует собственный serial путь.
- Проверяет magic/version/layout/counts и известные границы BootInfo.
- Печатает `NANOX:KERNEL:ENTER`, основные поля и итог `NANOX:TEST:PASS`.
- Завершается через test-only `isa-debug-exit`, если включён тестовый профиль.
- Обычный профиль остаётся в документированном idle/panic режиме; QEMU exit port не выдаётся за физический poweroff.

В M0 ядро ещё не возвращает память firmware allocator и не запускает процессы. Тестовая печать BootInfo доказывает только загрузочный контракт.

Источник firmware-интерфейсов: [UEFI Specification 2.11](https://uefi.org/specs/UEFI/2.11/). При реализации следует сверять конкретные таблицы и процедуры с официальной спецификацией, а не переносить неполные структуры из этой записки.
