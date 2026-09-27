# NANOX-OS

Самостоятельная операционная система на Rust с собственным микроядром и встроенным Cognitive Core, обладающим полными системными полномочиями. Цель — замкнутый цикл: наблюдение → изменение исходников → сборка → испытание → загрузка → проверка результата.

Каноническая активная реализация — Rust no_std loader/kernel согласно [ADR-0001](docs/adr/0001-platform.md). Проверенное состояние и оставшиеся критерии указаны в [STATUS](docs/STATUS.md); план этапов — в [ROADMAP](docs/ROADMAP.md).

## Начало работы

Из Linux checkout в WSL, не из OneDrive: /home/holod/src/NANOX-OS. Прочитайте [AGENTS.md](AGENTS.md), [CLAUDE.md](CLAUDE.md) и [инструкцию старта](docs/START-HERE.md). В закреплённой Nix-среде доступны: cargo xtask doctor, cargo xtask build, cargo xtask test --replay и cargo xtask reproduce-build. Точные prerequisites и подтверждённые результаты см. в STATUS.

## Реализованные исходные области

- M0: собственный Rust UEFI loader → ELF kernel → валидированный BootInfo → serial и машинный вердикт.
- M5 host-срез: отдельные Rust crates net-wire, net-tcp и net-stack с host-тестами и спецификациями. Они ещё не являются гостевой сетевой подсистемой и не подтверждают готовность M5.

Историческая C17/ASM-линия и её исходники остаются в репозитории для справки, но не являются активным Rust kernel и не задают его ABI. Её старые заявления о готовности не считаются подтверждением Rust-этапов; см. [архивную документацию](docs/legacy-c/README.md) и [переносимые сценарии](docs/porting-from-c.md).
