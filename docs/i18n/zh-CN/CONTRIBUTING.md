# 参与 PassportSim 贡献

[English](../../../CONTRIBUTING.md) | **简体中文** | [日本語](../ja/CONTRIBUTING.md) | [Français](../fr/CONTRIBUTING.md)

感谢你的帮助。PassportSim 的目标是与真实开发板的行为完全一致，因此评判一项改动要看证据：每一种行为都应能追溯到文档、许可证兼容的源码，或在芯片上的实测。

请遵守[行为准则](CODE_OF_CONDUCT.md)。安全问题请按 [SECURITY.md](SECURITY.md) 的说明报告，不要提交公开 issue。

最有价值的报告是**保真度差异**：同一份固件在模拟器和开发板上表现不同。请附上镜像(或其构建方法)、你执行的命令，以及模拟器和开发板各自的表现。不要附带任何真实设备数据(见[机密数据](#机密数据))。

## 环境准备

需要 [Rust](https://rustup.rs)(`rust-toolchain.toml` 中的工具链会自动安装)、[Bun](https://bun.sh) 1.4.1 或更新版本(CI 使用 `.bun-version` 中的版本)和 [just](https://github.com/casey/just)。开发主机为 Apple 芯片的 macOS，以及 Windows 10 或更新版本(x64)。

```sh
just setup                        # wasm 目标和网页的依赖包
cargo xtask secrets-check --init  # 每个克隆执行一次
cargo xtask hooks install         # pre-commit 和 pre-push 的机密检查
```

每个新的 worktree 都要重新执行 `just setup`：`web/node_modules/` 不共享，缺少它时网页测试会报找不到依赖包。

`just` 会列出全部命令。日常最常用的是 `just run`(构建并启动网页)、`just test`、`just check` 和 `just ci`。

## 提交 pull request 之前

```sh
just check              # cargo fmt、clippy、TypeScript 类型检查
just test               # Rust 和网页的单元测试
cargo xtask codegen     # 重新生成表格和文档；工作树必须保持干净
just ci                 # T0 层级：lint、全部测试和仓库检查
```

`just ci` 运行 `cargo xtask ci t0`，它不需要固件语料库或设备数据，在 macOS 和 Windows 上行为相同，测试的是所在的机器。`cargo xtask ci t1` 和 `t2` 增加语料库、golden、浏览器测试、oracle 对比和性能基准，在 macOS 上运行。GitHub Actions 覆盖两种主机：`pr-check.yml` 在 macOS 和 Windows 上检查每个 pull request，`release.yml` 发布版本，`deploy-web.yml` 部署网页。发布时在 Actions 页签运行 Release 工作流，可以指定版本号或递增位(默认 patch)：它会打标签、构建两个平台的包、发布带自动生成说明的 GitHub Release，并部署网页。

检查清单：

- [ ] `just ci` 通过；如果改动了模拟行为或网页，且你有语料库，`t1` 也要通过；
- [ ] 行为改动附带一个改动前会失败的测试；
- [ ] 没有手动编辑任何生成文件；
- [ ] diff 中没有真实设备数据。

## 提交信息

- 标题简短、使用英文，并带有约定式前缀：`feat(scope):`、`fix(scope):`、`perf(scope):`、`test(scope):`、`docs(scope):`、`ci:`、`build:`。
- 正文说明原因；行为改动要写明证据：spec 行、探针采集记录或文档章节。
- 只用普通的 `git commit` 提交。不要跳过或重定向 hook(`--no-verify`、`core.hooksPath`)。

## 净室规则

PassportSim 采用 MIT 许可证，完全基于公开信息编写。为保持这一点：

- **不要阅读或复制以下项目的源码：** QEMU(包括 Espressif 的分支)、esp32sim、ESP-EMU、NimBLE、Bumble、Zephyr、BlueZ、esptool、serialport-rs，以及其他任何 GPL、LGPL、AGPL 或无许可证的模拟器或协议栈。也不要转述它们的代码、结构或注释。
- **其他模拟器只能作为黑盒 oracle 运行：** 可以把它们的输出(控制台文本、寄存器写入序列、trace)与我们的对比，就像 `tools/oracle/` 和 `xtask oracle` 所做的那样，但绝不查看其内部。
- **寄存器和时序信息可以来自：** Espressif 的公开文档(ESP32-C3 技术参考手册和数据手册)、ESP-IDF 源码(Apache-2.0)、内置的 ROM ELF(Apache-2.0)，以及我们自己在真实芯片上运行的探针固件(`probes/`)。
- **注明来源。** `specs/` 中的每一行都有 `provenance` 字段，每个模型文件的头部都写明它实现的 spec 行或文档。尚未验证的假设标记为 `UNVERIFIED`。`cargo xtask provenance` 会在 T0 中检查这些。

## 文件位置

| 内容 | 位置 |
|---|---|
| 设计 | [ARCHITECTURE.md](ARCHITECTURE.md) |
| 单元测试 | 与代码放在一起(`#[cfg(test)]`)，以及各 crate 的 `tests/` |
| 启动真实镜像的集成测试 | `tests/milestones/`(名为 `t1_*` 或 `t2_*` 的测试在对应的 CI 层级运行) |
| golden、场景、transcript | `tests/golden/`、`tests/scenarios/`、`tests/transcripts/` |
| 网页单元测试 | `web/src/**/*.test.ts`(`bun test`) |
| 浏览器测试 | `web/tests/*.spec.ts`(`just e2e`) |
| 行为数据 | `specs/`(见 [specs/README.md](../../../specs/README.md)) |

## 约定

- **生成文件绝不手动编辑。** 修改源文件后运行 `cargo xtask codegen`(寄存器表、保真度文档)或 `cargo xtask docs`(命令参考)。
- **注册表是分散的。** 外设只改它自己的文件，不改 `periph/mod.rs`；命令通过 `#[command]` 自行注册。
- **冻结接口单独修改。** [ARCHITECTURE.md](ARCHITECTURE.md#冻结接口) 中列出的核心 trait 和 wasm ABI 要在单独的 pull request 中修改，先于使用它们的代码合入，并在描述中写明原因。
- **依赖需要审查。** 新的 crate 放在一个单独的小改动中，且 `cargo deny check` 通过；只允许 `deny.toml` 中列出的许可证。
- **核心 crate 不依赖宿主。** 它们能编译到 `wasm32-unknown-unknown`，不使用任何时钟、线程、文件、环境变量、网络或进程 API。宿主相关代码放在 `pemu_host::platform`，目录角色放在 `pemu_host::paths`。
- **Rust 2024 edition 和 `cargo fmt`。** 代码、注释和提交信息使用英文。注释简短，说明原因；代码本身说明做了什么。
- **仓库只用 LF 换行**，每个路径都必须在 Windows 上合法；`cargo xtask portable` 会检查这两点。

## 机密数据

真实设备数据绝不能进入仓库：不能有 MAC 地址、唯一 ID、校准值、flash 或 eFuse 转储、备份文件名或卡片内容。示例中的 MAC 使用 `02:00:00` 前缀的占位地址。上面安装的 hook 会拒绝包含此类数据的提交。完整策略见 [secrets.md](secrets.md)。

## 许可证

提交贡献即表示你同意你的贡献以本仓库的 MIT 许可证([LICENSE](../../../LICENSE))授权。
