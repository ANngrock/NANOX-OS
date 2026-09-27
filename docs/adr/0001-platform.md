# ADR-0001: платформа и архитектура для начала реализации

Дата: 2026-09-22. Статус: выбранное архитектурное решение для старта по запросу пользователя. Замещает предложения о C-ядре и обязательной собственной ISA до загрузки ОС. Не утверждает, что среда уже испытана.

## Решение

| Область | Выбор |
| --- | --- |
| Имя | NANOX-OS |
| Цель | Собственная ОС, управляемая встроенным полноправным ИИ, с циклом изменения самой системы |
| Первая машина | QEMU, versioned q35, OVMF, x86-64, один vCPU, 512 MiB RAM |
| Базовый accelerator | TCG, один execution thread; KVM/WHPX не обязательны и не используются для replay |
| CPU profile | Фиксированная модель `qemu64` и явно записанные features; без `host`/`max` |
| Устройства | COM1; загрузочный диск через firmware. С M3 COM2; runtime virtio-blk/net по этапам |
| Языки | Rust stable; `no_std` loader/kernel; Rust userspace; небольшой ASM-слой |
| Guest dependencies | Собственные workspace-crates; sysroot `core`, позже `alloc`, compiler runtime. Нет сторонних прикладных crates в loader/kernel |
| Host dependencies | Разрешены, фиксируются и отделяются от guest dependency graph |
| Загрузка | Собственный UEFI application, собственные bindings, `ExitBootServices`, собственный handoff |
| Executables | PE/COFF loader; статический ELF64 kernel; сначала статические ELF64 userspace-программы |
| Ядро | Микроядро: память, задачи, IPC, объекты, права, IRQ, таймеры, привилегированные механизмы |
| Root authority | Ядро создаёт bootstrap authority и отдаёт её доверенному init; init передаёт системные полномочия Cognitive executor |
| IPC | Синхронный bounded request/reply и notifications; shared-memory bulk transfer позже |
| ABI | Versioned схема и семантика; handoff v1 в M0, исполняемый syscall ABI и генератор в M2 |
| Cognitive Core | Системные процессы, отдельный полноправный исполнитель, сменные модели |
| Bridge | COM2: собственный framed NCI transport; модельный API — на стороне host adapter |
| Provider | Provider-neutral интерфейс; OpenAI-compatible и другие конкретные адаптеры проверяются отдельно |
| Build environment | WSL2 Ubuntu, Linux filesystem, Nix flake |
| Toolchain targets | `x86_64-unknown-uefi`, `x86_64-unknown-none`, host `x86_64-unknown-linux-gnu` |
| Закрепление версий | `flake.lock`, точный Rust toolchain, `Cargo.lock`, versioned machine и firmware hashes |
| CLI | `cargo xtask doctor/build/run/test/debug`; replay отдельным test-профилем |
| Артефакты | RunRecord с source/toolchain/machine hashes, argv, raw logs, exit status и verdict |
| Первый milestone | M0 включает стенд и вертикальную загрузку; M1 — полноценные memory/interrupt механизмы |
| Исследования | IR, доказательства, replay runtime, собственная ISA/toolchain после базовой системы |

## Исправления предложенного пакета Claude

1. `init` не создаёт полномочия из ничего: доверенное право возникает в ядре и передаётся при bootstrap.
2. «Одна точка недетерминированности» означает согласованные интерфейсы и трассировку. Все физические DMA-записи нельзя превратить в один syscall; replay определяется конкретной моделью исполнения.
3. Равенство serial-логов — критерий конкретного record/replay эксперимента, не доказательство обратимости произвольного железа.
4. Один `flake.lock` не фиксирует всё, если Rust или артефакты скачиваются по подвижной ссылке вне flake.
5. COM2 несёт NCI frames. HTTP/provider tools protocol не объявляется системным ABI.
6. Совместимость с одним OpenAI-compatible endpoint не означает совместимость с любым API.
7. WSL/USB не меняются по историческому номеру диска. NANOX M0 работает на виртуальных файловых образах.
8. Весь будущий ABI нельзя обоснованно заморозить до реализации. Фиксируются текущие версии, расширения вводятся явно.

## Последствия

Rust уменьшает ряд ошибок владения памятью, но не доказывает корректность unsafe, схемы полномочий, драйверов или ИИ. Собственные UEFI bindings увеличивают объём ручной проверки layout и calling conventions.

Полные полномочия Core сохраняются. Изоляция приложений и сервисов не даёт изоляции от доверенного Sovereign. Внешняя модель влияет на решения привилегированного исполнителя, даже если её API не соединён напрямую с ядром.

Сложные сервисы сначала создаются внутри ограниченного стенда. Отсутствие сторонних crates в ядре не распространяется автоматически на весь userspace: дополнительные зависимости оформляются отдельно, без молчаливой замены собственных системных компонентов.

Выбранные Rust targets документированы в [bare-metal target](https://doc.rust-lang.org/rustc/platform-support/x86_64-unknown-none.html) и [UEFI targets](https://doc.rust-lang.org/rustc/platform-support/unknown-uefi.html). UEFI entry использует `efiapi`, kernel entry — собственный документированный handoff.

## Когда пересматривать

Новый ADR требуется для смены ядра/языка, изменения модели полномочий Core, target architecture, wire ABI с несовместимыми изменениями, основной среды сборки или границы внешних зависимостей. Конкретные версии доступных инструментов выбираются в M0 и фиксируются без повторного архитектурного опроса.
