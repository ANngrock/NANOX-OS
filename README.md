# NANOX-OS run evidence (2026-10-06)

Small evidence files (every file up to 1 MiB: run records, inputs, source manifests, logs,
serial output, configs, stage evidence) from two working trees, before their heavy and
regenerable parts were deleted to free disk space:

| Archive | Source | sha256 |
|---|---|---|
| `graphics-evidence.tar.xz` | `NANOX-OS-graphics/out` (branch `codex/first-graphical-boot`) | 94bcd810ef3255d54fb47dc2b6d501d3633347e379e7d71400b9b666016dcdbd |
| `m9-evidence.tar.xz` | `NANOX-OS-m9/out` | 36702bd957d29cf67dcb4c7e18ada5c9ea68b2d162cc37d7ac895ea573d75c57 |

`INDEX.json` inside each archive lists the runs kept complete locally (the newest run of each
kind), and every directory and file that was deleted (stage `checkout/` copies of the
repository, disk images, firmware copies, replay recordings and other files over 1 MiB).
Runs cited in the docs keep their KERNEL.ELF, USER.ELF and BOOTX64.EFI locally.
The earlier archive of `NANOX-OS/out/runs` is on branch `archive/run-evidence-20261003`.
