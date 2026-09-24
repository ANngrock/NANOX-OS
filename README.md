# NANOX-OS

Самостоятельная операционная система на Rust с собственным микроядром и встроенным Cognitive Core, обладающим полными системными полномочиями. Цель — замкнутый цикл: наблюдение → изменение исходников → сборка → испытание → загрузка → проверка результата.

**Реализован вертикальный boot M0: свой Rust UEFI loader → ELF-ядро → BootInfo → serial и машинный вердикт. Результаты проверок и готовность этапа — в [STATUS](docs/STATUS.md).**

## Начало работы

Claude Code и другим агентам: прочитать [CLAUDE.md](CLAUDE.md), [AGENTS.md](AGENTS.md), затем [инструкцию старта](docs/START-HERE.md). Первое задание — завершить M0: воспроизводимый стенд и реальная загрузка своего ELF-ядра через свой UEFI loader.

| Документ | Назначение |
| --- | --- |
| [Архитектурное решение](docs/adr/0001-platform.md) | Принятый для старта набор технологий и исправления предыдущих предложений |
| [Реализация M0](docs/M0-IMPLEMENTATION.md) | Загрузчик, ядро, ABI, формат образа и команды |
| [Рабочая среда](docs/ENVIRONMENT.md) | Основной Linux checkout, Nix и локальная настройка TLS |
| [Replay M0](docs/M0-REPLAY.md) | Проверенный профиль, ограничения и патч QEMU |
| [Карта проекта](docs/PROJECT-MAP.md) | Каталоги, модули, зависимости, границы ответственности |
| [План и критерии готовности](docs/ROADMAP.md) | M0–M10 и исследовательская ветка |
| [Boot M0](docs/specs/BOOT-M0.md) | Контракт загрузчика и ядра |
| [ABI и NCI](docs/specs/ABI-NCI.md) | Начальные соглашения объектов, системных вызовов и действий ИИ |
| [Проверки и replay](docs/specs/TESTING-REPLAY.md) | Воспроизводимость, артефакты, коды завершения и ограничения |
| [Статус](docs/STATUS.md) | Что проверено и что предстоит реализовать |

Основной checkout: `/home/holod/src/NANOX-OS` в WSL Ubuntu. Исходный OneDrive-каталог сохранён как входной снимок. Сборка выполняется на Linux filesystem через Nix: Rust 1.90.0, QEMU 9.2.4 с патчем replay exit code, `pc-q35-9.2`, OVMF 202411, x86-64, один vCPU.

```sh
cd /home/holod/src/NANOX-OS
nix develop
cargo xtask doctor
cargo xtask build
cargo xtask run
cargo xtask test --replay
cargo xtask reproduce-build
```

Образ: `out/nanox.img`. Доказательства: `out/runs/`; каждый запуск сохраняет сырые логи, argv, версии, hashes и вердикт. Отладка сохранённого запуска: `cargo xtask debug --run <run-id>`, затем GDB с `hbreak kernel_main`. Первый вход в среду включает сборку патченного QEMU; [ADR-0002](docs/adr/0002-qemu-replay-exit.md) объясняет причину.

Старый корневой `ARCHITECTURE.md` заменён по содержанию этим комплектом; его существовавшее удаление из рабочего дерева сохранено. Актуальный выбор языка — Rust, а не C из предыдущей записки.
