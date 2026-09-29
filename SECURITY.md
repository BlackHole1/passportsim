# Security Policy

**English** | [简体中文](docs/i18n/zh-CN/SECURITY.md) | [日本語](docs/i18n/ja/SECURITY.md) | [Français](docs/i18n/fr/SECURITY.md)

## Supported versions

PassportSim is pre-1.0. Security fixes land on `main` and in the next package; older packages are
not patched.

## Reporting a vulnerability

Do not report security issues in public issues or pull requests. Email **bh@bugs.cc** with:

- what the issue is and what an attacker could do with it;
- the steps, input or firmware image that reproduce it (with no real device data, see below);
- the version (`passportsim --version`) and your host OS.

You will get an acknowledgement within 3 working days and an assessment within 10. We will keep you
informed, agree a disclosure date with you, and credit you unless you prefer otherwise.

## In scope

The emulator runs untrusted firmware and, on request, talks to a real device. These boundaries are
security relevant:

- **The guest boundary.** Firmware must not read or write host files, reach the network beyond what
  the virtual LAN explicitly bridges, or crash the host process in a way that affects other
  instances.
- **The daemon.** `passportsim serve` listens on loopback only, and every HTTP, WebSocket and MCP
  request needs its owner-only token or the web UI's one-time launch code. A way around either is a
  vulnerability.
- **The real-device flasher.** `flash_device` backs up the flash first and never writes or erases
  the regions that identify the device, never erases the whole chip and never burns eFuses. Any
  input that makes it do so is a vulnerability.
- **Secret material.** Device data (MAC addresses, unique IDs, card contents, flash or eFuse dumps,
  backup names) must never appear in tool output, logs, artifacts, the repository or a package.
- **Packages.** A package must not carry the build machine's account paths or anything the secrets
  guard refuses.

## Out of scope

- Bugs in the emulated firmware, or differences between the emulator and the board. These are
  fidelity issues; open a normal issue.
- Attacks that need code already running as your user on the host.
- Firmware that loops forever or exhausts its own memory; `run` takes a time budget.

## Device data in reports

Never attach flash dumps, eFuse dumps, backups or logs from a real device. If a report needs them,
say so and we will agree a private channel. See [docs/secrets.md](docs/secrets.md).
