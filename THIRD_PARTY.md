# Third-party material

The PassportSim source code is MIT licensed (see `LICENSE`). This file lists third-party material
that the repository contains, or that its tools and data are derived from. Rust crate
dependencies are governed separately by `deny.toml`.

## Espressif esp-rom-elfs (Apache-2.0): `assets/rom/`

- Files: `assets/rom/esp32c3_rev101_rom.elf`, `assets/rom/esp32c3_rev3_rom.elf`, redistributed
  unmodified and embedded by `pemu-loader` behind the default feature `bundled-rom`.
- Copyright (c) Espressif Systems (Shanghai) Co., Ltd.
- License: Apache License 2.0, full text in `assets/rom/LICENSE` (verbatim upstream copy).
- Source: <https://github.com/espressif/esp-rom-elfs>, release `20241011`; SHA-256 digests in
  `assets/rom/pins.toml`.
- Notices: `assets/rom/NOTICE`. The mask ROMs include code compiled from third-party software
  listed in the "ROM Source Code Copyrights" section of the ESP-IDF "Copyrights and Licenses"
  page.

## ESP-IDF (Apache-2.0)

- Copyright (C) 2015-2023 Espressif Systems, licensed under the Apache License 2.0
  (<https://github.com/espressif/esp-idf/blob/v5.5.3/LICENSE>). Third-party components of
  ESP-IDF carry their own notices, listed in ESP-IDF `docs/en/COPYRIGHT.rst`.
- Used as: the reference for register addresses, field layouts, interrupt source numbers and
  symbol names recorded with citations in `specs/` (ESP-IDF v5.5.3); the SDK that probe firmware
  under `probes/` is built with; the source of the pinned binary-blob symbol list under
  `specs/hle/`. Where a file in this repository reproduces ESP-IDF definitions, it names the
  ESP-IDF file it cites.

## Firmware linked into the committed probe ELFs: `tests/fw/*.elf`

The stripped probe ELFs under `tests/fw/` (`probes/README.md`) are
firmware we build from `probes/` with ESP-IDF v5.5.3. Beside our own MIT code they contain
object code from the following, under the licences ESP-IDF v5.5.3 declares for them in
`docs/en/COPYRIGHT.rst` and in each component's own licence file. Which of them a given ELF
contains depends on what the probe links; the radio probes `scan3`, `pkgatt`, `probe_wifi_conn`
and `probe_wifi_http` are the ones with Wi-Fi, BLE and PHY code.

| Material | Licence as declared by ESP-IDF v5.5.3 | Declared in |
|---|---|---|
| ESP-IDF components (startup, drivers, HAL, heap, `esp_timer`, `nvs_flash`, `esp_netif`, `esp_http_client` and the rest) | Apache-2.0, Copyright (C) 2015-2023 Espressif Systems | `LICENSE`, `docs/en/COPYRIGHT.rst` |
| Espressif binary libraries: `esp_wifi/lib/esp32c3` (`libcore`, `libnet80211`, `libpp`, `libmesh`, `libespnow`, `libsmartconfig`, `libwapi`), `esp_phy/lib/esp32c3` (`libphy`, `libbtbb`), `esp_coex/lib/esp32c3` (`libcoexist`), `bt/controller/lib_esp32c3_family/esp32c3` (`libbtdm_app`) | Apache-2.0 | the `LICENSE` file in each of those `lib` directories |
| FreeRTOS kernel (original parts) | MIT, Copyright (C) 2017 Amazon.com, Inc. or its affiliates | `freertos/FreeRTOS-Kernel/LICENSE.md` |
| lwIP (original parts) | BSD, Copyright (C) 2001, 2002 Swedish Institute of Computer Science | `lwip/lwip/COPYING` |
| wpa_supplicant | BSD, Copyright (C) 2003-2022 Jouni Malinen and contributors | `docs/en/COPYRIGHT.rst` |
| FreeBSD net80211 (in the Wi-Fi libraries) | BSD, Copyright (C) 2004-2008 Sam Leffler, Errno Consulting | `docs/en/COPYRIGHT.rst` |
| Mbed TLS | Apache-2.0, Copyright (C) 2006-2018 ARM Limited | `mbedtls/mbedtls/LICENSE` |
| mynewt-nimble (BLE host) | Apache-2.0, Copyright (C) 2015-2018 The Apache Software Foundation | `bt/host/nimble/nimble/LICENSE` |
| TLSF allocator | BSD 3-clause, Copyright (C) 2006-2016 Matthew Conte | `docs/en/COPYRIGHT.rst` |
| HTTP Parser (`probe_wifi_http`) | NGINX and Joyent terms | `http_parser/LICENSE.txt` |
| Newlib C library (from the `riscv32-esp-elf` toolchain) | BSD, copyright held by the respective parties | `newlib/COPYING.NEWLIB` |
| `libgcc` (from the `riscv32-esp-elf` toolchain, esp-14.2.0_20251107) | GPL-3.0 with the GCC Runtime Library Exception, which permits distributing the resulting binary under any terms | GCC `COPYING.RUNTIME` |

The Espressif binary libraries are redistributed only inside those ELFs, as linked object code,
and never as separate archives.

## Passport Keys BLE sources (MIT): `probes/pkgatt/main/`

- Files: `probes/pkgatt/main/pk_ble.c`, `pk_ble.h`, `pk_protocol.c`, `pk_protocol.h`, byte-identical
  copies of the Passport Keys sources, unmodified, and object code built from them inside
  `tests/fw/pkgatt.elf`.
- Copyright (c) 2026 FoloToy
- License: MIT, full text in `probes/pkgatt/main/LICENSE.passport-keys` (verbatim copy of the
  Passport Keys `LICENSE`).
- `tests/fw/pkgatt.elf` is a probe we build from these MIT sources and our own `pkgatt.c`, not a
  FoloToy product firmware binary, so the rule against committing FoloToy firmware binaries does
  not apply to it and it may be committed.

## FoloToy AI Passport BSP demo (MIT): shipped in packages, never committed

- Files: the prebuilt `official` demo image and its ELF (corpus id `official`), embedded by
  `cargo xtask package` and by the static web bundle as the firmware a first run boots. They are **not in this repository**: the
  packaging step reads them from the host's corpus, checks them against the pinned SHA-256
  prefixes, and refuses to embed anything that does not match.
- Copyright (c) FoloToy. License: MIT.
- Source: the upstream BSP demo at commit `f75873f`, whose id and full MIT text ship beside the
  image in every package (`payload/firmware/official-demo.LICENSE` and `.NOTICE`), so a package
  never carries the image without its licence and its commit id.
- A host without the corpus gets a package without the demo and a receipt that says so, rather
  than a failed build.

## riscv-tests (BSD)

- Copyright (c) 2012-2015, The Regents of the University of California (Regents). Licensed under
  the BSD 3-clause license of <https://github.com/riscv-software-src/riscv-tests>.
- Used as: the RISC-V ISA conformance suite run by `cargo xtask riscv-tests` against `pemu-rv32`.
  Reviewed test ELFs committed under `crates/pemu-rv32/tests/data/riscv-tests/` keep this license, and their provenance is recorded in
  `crates/pemu-rv32/tests/data/riscv-tests/MANIFEST.toml`: upstream commit
  2ebecad997fa58cd9e5724340ba75aa4b59bd1d0, `env` submodule (riscv-software-src/riscv-test-env)
  commit 6de71edb142be36319e380ce782c3d1830c65d68. `xtask/src/riscv_tests/c3_env_p.h` redefines two
  macros of the upstream BSD-3-Clause `env/p/riscv_test.h` and reuses upstream macro bodies
  verbatim, so it carries this license too.

## Web UI libraries: `web/` and the built `main.js` and `styles.css`

The web page is built with the libraries below. `bun run build` bundles them
into `main.js`, and the Tailwind CLI compiles `styles.css`; both files ship in every package
(`payload/web/`) and in the static web bundle. Versions are pinned in `web/package.json` and
`web/bun.lock`; nothing is fetched at run time.

- **coss ui components**, MIT (`apps/ui/package.json` declares `"license": "MIT"`; no copyright line is given, so it is the coss contributors'). Source:
  <https://github.com/cosscom/coss>, directory `apps/ui/` at commit
  `8423f18a8b4830e875def4a7400730f29cae1147`, which that repository's `LICENSING.md` places under
  MIT (the rest of the repository is AGPL-3.0, and nothing outside `apps/ui/` is copied). Vendored
  files: `web/src/ui/*.tsx` from `apps/ui/registry/default/ui/` and `web/src/ui/lib/*.ts` from
  `apps/ui/registry/default/lib/`, with only their import paths changed; the colour tokens in
  `web/src/styles.css` are the neutral theme of `apps/ui/registry/registry-styles.ts`.
- **React** and **React DOM** 19.3.0, **scheduler** 0.28.0, **use-sync-external-store** 1.7.0: MIT,
  Copyright (c) Meta Platforms, Inc. and affiliates.
- **Base UI** (`@base-ui/react` 1.8.0, `@base-ui/utils` 0.4.0): MIT, Copyright (c) the Base UI
  contributors (MUI).
- **Floating UI** (`@floating-ui/core`, `dom`, `react-dom`, `utils`): MIT, Copyright (c) Floating UI
  contributors.
- **reselect** 5.3.0 and **@babel/runtime** 7.29.7 (Base UI dependencies): MIT.
- **lucide-react** 0.555.0 (icons): ISC, Copyright (c) Lucide Contributors; parts derived from
  Feather, MIT, Copyright (c) 2013-2023 Cole Bemis.
- **class-variance-authority** 0.7.1: Apache-2.0, Copyright (c) Joe Bell.
- **clsx** 2.1.1: MIT, Copyright (c) Luke Edwards. **tailwind-merge** 3.4.0: MIT, Copyright (c)
  Dany Castillo.
- **Tailwind CSS** 4.3.3 (build tool; its base styles are compiled into `styles.css`): MIT,
  Copyright (c) Tailwind Labs, Inc.

## AI Passport product photo (MIT): `web/src/app/view/device-front.webp`

- The device the web page draws. Derived from `docs/brand/ai-passport-front.png` of the FoloToy
  ai-passport repository (<https://github.com/FoloToy/ai-passport>; source file SHA-256
  `d813ba17e2821ecfff5bb3070605b1960d5c2501bd2c6dcf6e95d9a70be4f122`): cropped to the device's
  outline, its placeholder screen content painted black, and re-encoded as WebP. `bun run build`
  inlines it into `main.js`, so it ships wherever `main.js` does.
- License: MIT. The ai-passport `LICENSE`, verbatim (this file ships in every package and web
  bundle, so the notice travels with the image):

```text
MIT License

Copyright (c) 2026 FoloToy

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
