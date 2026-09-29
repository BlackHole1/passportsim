# 快速入门

[English](../../quickstart.md) | **简体中文** | [日本語](../ja/quickstart.md) | [Français](../fr/quickstart.md)

发布包就是一个压缩包，里面只有一个可执行文件。它不需要 ESP-IDF、`~/.espressif` 工具目录、固件语料库或设备，也不需要 Bun、Node、Python、QEMU，更不需要下载 ROM。

## 支持的主机

Apple 芯片上的 macOS 27 或更新版本(`aarch64-apple-darwin`)，以及 x64 上的 Windows 10 1903 或更新版本(`x86_64-pc-windows-msvc`，见第 8 节)。不支持 Linux。

## 1. 获取发布包

安装发布版：macOS 上运行 `curl -fsSL https://passportsim.bugs.cc/install.sh | sh`，Windows 上在 PowerShell 中运行 `irm https://passportsim.bugs.cc/install.ps1 | iex`。它安装到用户目录，并说明如何运行 `passportsim`。也可以在源码检出目录中构建发布包，这种方式没有安装程序，也不需要修改 `PATH`：

```sh
cargo run -q -p xtask -- package --target aarch64-apple-darwin
```

它会在 `target/package/` 下写入：

| 路径 | 内容 |
|---|---|
| `passportsim-0.1.0-macos-arm64/` | 解压后的发布包 |
| `passportsim-0.1.0-macos-arm64.tar.gz` | 同一目录打成的压缩包 |
| `passportsim-0.1.0-web/` | 静态网页包(第 5 节) |
| `passportsim-0.1.0-web.tar.gz` | 同一网页包打成的压缩包 |

发布包中的任何文件都不会包含构建账户的信息：源码路径会被替换为固定标记，如果任何地方残留用户名或主目录，打包就会失败。

把压缩包解压到任意位置并进入该目录。下面的所有命令都在这个目录中执行：

```sh
cd passportsim-0.1.0-macos-arm64
```

如果打包中途失败，先检查磁盘空间：它会写入两个目录和两个压缩包。删除 `target/package/` 后重新运行即可；它总是从头重建。

### 只要可执行文件

不打包，直接在源码检出目录中运行模拟器：

```sh
cargo build -p pemu-cli
./target/debug/passportsim status
```

把下文中的 `./passportsim` 都换成 `./target/debug/passportsim`。第 2 节不适用；这样构建出的程序不带 payload(没有内嵌演示固件)，`--version` 会说明这一点(第 6 节)。

## 2. 在 macOS 上首次启动

可执行文件**未签名，也未经公证**。如果压缩包是通过浏览器下载的，或来自另一台机器，需要先清除一次隔离属性：

```sh
xattr -dr com.apple.quarantine .
```

这条命令不会安装任何东西；如果发布包从未离开过本机，它什么也不做。

## 3. 确认可以运行

```sh
./passportsim status
```

它会输出实例列表(刚启动时为空)、产物目录和运行回执：

```text
no instance is running
artifacts: ~/Library/Application Support/passportsim/artifacts
profile fast | deterministic
```

目录只在需要写入时才会创建。

每条命令都支持 `--help`，相同的内容也在 [commands/](../../commands/index.md) 中：

```sh
./passportsim --help
```

任何命令加上 `--output json` 都会输出 JSON。(`--json` 用于给命令传入*输入*文档。)

文本输出的长度有上限：相同的行会合并，过长的行以 `...(+N chars)` 结尾，只保留前 10 条和后 30 条，中间用一行 `... N lines elided ...` 标记。要读取较长的串口输出，可以用 `serial read --max-bytes` 配合每次读取返回的 `next_cursor` 分段读取。

```sh
./passportsim status --output json
```

### 守护进程与 MCP

实例运行在守护进程中。`start` 在没有守护进程时会在后台启动一个(`passportsim serve --headless`)，之后的命令(`run`、`serial`、`status`、`stop` 等)都发送给它，因此实例在启动它的命令结束后仍然存在。没有守护进程时，命令在自己的进程中运行；`--ephemeral` 可以强制这样做。相对的固件路径在发送前会转换为绝对路径(通过 MCP 和 HTTP 时路径必须是绝对路径)。下面的 `pk` 是一个语料库 id(第 4 节)。

```sh
./passportsim start pk --boot none
./passportsim run 'serial:/bsp_i2c/'
./passportsim serial read --cursor 0
./passportsim stop
./passportsim serve --stop
```

守护进程只监听 `127.0.0.1:8765`(被占用时改用空闲端口)。每个请求都需要它写在 `~/.passportsim/` 中 `serve.json` 旁边的令牌，该文件仅属主可读。无界面的守护进程把日志写到 `~/.passportsim/logs/serve.log`，在没有实例 10 分钟后自动退出。

不带 `--headless` 的 `passportsim serve` 在前台运行，输出它的 URL、令牌文件路径和一个指向网页(第 5 节)的 `ui:` 链接。链接在 `#lc=` 之后带有一次性启动码，60 秒内有效。`passportsim serve --stop` 会先停止实例并写出它们的产物，然后停止守护进程。

`passportsim mcp` 是供智能体使用的 MCP 服务：通过标准输入输出通信，并转发给守护进程(需要时自动启动)。`--caps audio,nfc` 在核心工具集之外增加工具组。在客户端配置中填写可执行文件和这一个参数：

```sh
./passportsim mcp
```

## 4. 内置了什么

| 需要 | 内置 | 可选的替代 |
|---|---|---|
| ROM | Espressif ESP32-C3 的两个掩膜 ROM ELF，按 eFuse 中的芯片版本选择 | `start` 不使用任何替代(第 6 节) |
| eFuse | 合成的镜像：芯片版本 v1.1，占位 MAC `02:00:00:xx:xx:xx`，校准字为 0 | `--efuse-dump <dir>`，会使机器被标记为受污染([secrets.md](secrets.md)) |
| 固件 | 官方 BSP 演示固件(构建主机上有它时才内嵌，第 6 节) | 语料库 id，或 `idf.py` 构建目录、合并 bin、`.pebundle` 的路径 |
| 工具 | 这个可执行文件，或网页包 | ESP-IDF 只用于*构建*固件；esptool 只用于 USB Serial/JTAG 端点 |

`passportsim doctor` 会报告本机解析到的内容：内置 ROM 的校验值、ROM 替代设置、内嵌的演示固件，以及 `corpus.toml` 中每个条目是找到、缺失还是不匹配。它从不输出文件内容。

```sh
./passportsim doctor
```

也可以直接传入一份报告，智能体通过 MCP 就是这样调用的([commands/doctor.md](../../commands/doctor.md))：

```sh
printf '%s' '{"report":{"bundled_roms":[],"corpus":[]}}' | ./passportsim doctor --json -
```

不需要任何配置文件或语料库。

### 固件语料库

**语料库 id** 是本机为固件镜像起的短名称，这样 `start official` 就可以代替路径。id 不是内置的，每台机器在配置目录中的 `corpus.toml` 里自行定义：

| 角色 | macOS | Windows |
|---|---|---|
| 配置目录(`corpus.toml`、`config.toml`) | `~/.config/passportsim/` | `%APPDATA%\passportsim\` |
| 数据根目录(`corpus/`、`artifacts/`、`audio/`) | `~/Library/Application Support/passportsim/` | `%LOCALAPPDATA%\passportsim\data\` |

每个 id 一张表。文件键有 `bin`(合并的 flash 镜像)、`elf`(应用 ELF)、`boot_elf`(引导程序 ELF)和 `pt`(分区表)；只有 `bin` 是必需的。`sha256` 用完整的 64 字符摘要固定每个文件(这里做了缩写)：

```text
[official]
bin = "corpus/official/FoloToy-AI-Passport-8MB.bin"
elf = "corpus/official/FoloToy-AI-Passport.elf"
boot_elf = "corpus/official/bootloader.elf"
sha256 = { bin = "5802...e163", elf = "dd63...a2de", boot_elf = "5fcf...17a8" }
```

路径规则(同样适用于 `config.toml`)：

- **绝对路径**按原样使用(在 Windows 上，以 `/` 开头也视为绝对路径)；
- **`~/...` 或 `~\...`** 表示主目录；
- **其他路径都相对于数据根目录**，从不相对于当前目录，这样同一份 `corpus.toml` 可以在不同机器间通用。用 `..` 跳出数据根目录的路径会被拒绝。

文件缺失时报 `E_ASSET_MISSING`，摘要不匹配时报 `E_ASSET_HASH`；`doctor` 会列出被拒绝的路径。

环境变量：

- `PASSPORTSIM_CORPUS_<ID>` 替换单个条目的路径(`<ID>` 为大写，`-` 换成 `_`，例如 `probe-long` 对应 `PASSPORTSIM_CORPUS_PROBE_LONG`)。两个 id 映射到同一变量名时会被拒绝。
- `PASSPORTSIM_DATA_ROOT` 移动数据根目录。
- `PASSPORTSIM_HOME` 把所有目录角色移到 `<dir>/<role>/`。

本项目自己的测试使用这些 id：`official`(官方 BSP 演示固件)、`pk`(Passport Keys)、`goldminer`、`demo`，以及 ROM 和探针镜像 `rom0`、`probe-long`、`qemu-oracle`、`probe2`、`scan3` 和 `pkgatt`。**全新安装没有任何语料库 id**，这没有问题：不带参数的 `start` 会启动内置的演示固件，给出路径则启动你自己的镜像。

```sh
./passportsim start
./passportsim start ~/esp/my-project/build
./passportsim stop
./passportsim serve --stop
```

构建目录通过 `idf.py build` 写出的 `flasher_args.json` 读取，因此每个部分都会放到记录的偏移处。`cargo build` 构建的程序没有演示固件：此时不带参数的 `start` 会报 `E_ASSET_MISSING`，并提示使用 `cargo xtask package`。

## 5. 网页包

`passportsim-0.1.0-web/` 是一个静态站点：`index.html`、样式表、页面脚本、worker 和音频 worklet 脚本、内置两个 ROM 的 wasm 核心，以及构建主机上有演示固件时附带的演示 `.pebundle`。可以用任何静态文件服务器提供，放在站点根目录或子路径下(例如 `/emu/`)均可。直接从磁盘打开页面(`file://`)无法运行。

页面会启动演示固件。拖入 `idf.py` 构建目录、合并 bin 或 `.pebundle` 即可改为运行它；单独拖入的 ELF 只为 `inspect` 提供符号。

**简洁**模式(默认)显示设备、固件卡片，以及包含页面自身加载步骤的实时日志。**高级**模式增加运行控制，Console、UI tree、Events、Inspect、Fidelity 和 Perf 标签页，以及 Battery、USB、Audio、NFC、Wi-Fi、BLE 和 Snapshots 卡片。页面支持英文、简体中文、日文和法文，跟随浏览器语言和系统主题，并记住在页头所做的选择。`?mode=advanced`(或 `simple`)和 `?lang=ja`(或 `en`、`zh-CN`、`fr`)只对本次加载生效；配合 `serve` 链接使用时，要写在 `#` 之前：`http://127.0.0.1:8765/?mode=advanced&lang=ja#lc=<code>`。

同样的网页包也在发布包的 `payload/web/` 中，并内嵌在可执行文件里，`passportsim serve` 提供的就是它。

使用其他服务器时，必须发送 `Cross-Origin-Opener-Policy: same-origin` 和 `Cross-Origin-Embedder-Policy: require-corp`(否则没有 `SharedArrayBuffer`)，并以 `application/wasm` 类型提供 `.wasm`。网页包本身也是一个现成的 Cloudflare Workers 项目；命令和单文件 25 MiB 的限制见 [deploy-cloudflare.md](deploy-cloudflare.md)。

## 6. 已知限制

| 项目 | 现状 |
|---|---|
| 通过路径指定的固件的应用 ELF | `until_ui_settled`、`inspect`、`ui` 和 `--boot-cache` 需要应用 ELF 的 DWARF 信息。命令行只能为演示固件和带有 `elf` 条目的语料库 id 找到它。对于通过路径指定的合并镜像、构建目录或 `.pebundle`，`boot: until_ui_settled` 会耗尽全部预算并报告 `unobservable`，各种遍历工具会指出缺少 ELF，`--boot-cache` 会以 `E_STATE` 失败。网页则会读取拖入的构建目录或 `.pebundle` 中的 ELF |
| ROM 替代 | `doctor` 会检查 `PASSPORTSIM_ROM` 以及 `config.toml` 中的 `rom.rev101` / `rom.rev3`，但 `start` 总是启动内置 ROM；没有 `--rom` 参数 |
| 内嵌的演示固件 | 只有构建主机上有 `official` 语料库条目时才内嵌(镜像从不提交到仓库)；回执会说明是否内嵌。没有它时，不带参数的 `start` 会以 `E_ASSET_MISSING` 失败 |

### payload 与 `--version`

可执行文件内含完整的 payload：网页资源、wasm 核心、schema、文档、智能体技能和演示固件。单独把它移到别处也不会缺少任何东西。`--version`(或 `-V`)会输出 payload 摘要，即 `receipt.json` 中的 `payload.sha256`，并重新校验：

```sh
./passportsim --version
```

```text
passportsim 0.1.0
payload: embedded, sha256 <the payload.sha256 of receipt.json>
```

| `payload:` 行 | 含义 |
|---|---|
| `embedded, sha256 <digest>` | 打包的可执行文件，完好无损 |
| `package directory beside the binary, sha256 <digest>` | 没有内嵌 payload；使用旁边经过校验的 `payload/` 目录 |
| `none: development build ...` | 普通的 `cargo build`；该行其余部分说明如何获得带 payload 的版本 |
| `embedded, damaged: ...` | 可执行文件已被改动；请替换它 |

## 7. 回执

`receipt.json` 记录版本、提交、目标平台、每个 payload 文件的 SHA-256 和 payload 总摘要、是否内嵌了演示固件(以及未内嵌的原因)、每个 ROM ELF 是否确实包含在各个产物中，以及运行了哪些机密检查规则。它不包含主机路径、用户名或设备身份信息。

打包时会对每个文件运行机密检查([secrets.md](secrets.md))，命中即失败。哈希规则需要主机自己的 `~/.config/passportsim/secrets-check.toml`；没有该文件时，回执中为 `secrets.hashed_rules: false`。

发布包没有签名，也没有 Homebrew、winget 或 Scoop 软件包；第 2 节和第 8 节的首次启动步骤可以代替它们。

## 8. Windows

Windows 发布包是 `passportsim-0.1.0-windows-x64.zip`，其中是 `passportsim.exe`。它在装有 MSVC 工具的 Windows 主机上构建，在 PowerShell 中执行：

```powershell
cargo run -q -p xtask -- package --target x86_64-pc-windows-msvc --payload-from <macOS package directory>
```

它写出与第 1 节相同的四项输出，压缩包为 `.zip` 格式。可执行文件静态链接 C 运行时，因此运行它的机器无需安装任何东西；它还声明了长路径支持和 UTF-8 代码页。任一检查未通过时打包失败，两项检查结果都记录在 `receipt.json` 的 `windows` 部分。

**两种主机使用同一份 payload。** 演示固件只存在于 macOS 构建主机上，而 wasm 核心的字节在不同主机上会不同。`--payload-from` 从同一提交的 macOS 发布包中取出这两者，校验演示固件，自行构建其余部分，并且只有在整个 payload 与 macOS 发布包逐个文件一致时才打包；此时两份回执的 `payload.sha256` 相同。不使用该参数时，Windows 发布包使用自己的 wasm 核心，且不含演示固件。没有面向 Windows on Arm 的发布包。

**在 Windows 上首次启动。** `.exe` 没有签名，SmartScreen 会显示“Windows 已保护你的电脑”：点击**更多信息**，再点击**仍要运行**。也可以在 PowerShell 中清除下载标记：

```powershell
Get-ChildItem -Recurse | Unblock-File
```

之后，第 3 节和第 4 节的检查同样适用：

```powershell
.\passportsim.exe --version
```

```powershell
.\passportsim.exe status
```

```powershell
.\passportsim.exe doctor
```

其他操作与上文相同，只需把 `./passportsim` 换成 `.\passportsim.exe`。后台守护进程在启动它的控制台关闭后仍会运行；用 `passportsim serve --stop` 结束它。

目录角色来自 Windows 的已知文件夹(known folders)，而不是 `%USERPROFILE%` 或 `%LOCALAPPDATA%`。要移动所有目录，请设置 `PASSPORTSIM_HOME=<dir>`。

## 9. 下一步

- [commands/index.md](../../commands/index.md)：每条命令及其 MCP 工具、HTTP 路由和场景步骤(自动生成，英文)。
- [errors.md](../../errors.md)：全部错误码(自动生成，英文)。
- [SKILL.md](../../../skills/passportsim/SKILL.md)：智能体技能说明，包括设备安全禁用列表。
- [secrets.md](secrets.md)：机密数据策略。
- 仓库中的 `docs/ARCHITECTURE.md`：设计文档(不随发布包分发)。
