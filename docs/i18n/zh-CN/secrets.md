# 机密数据策略

[English](../../secrets.md) | **简体中文** | [日本語](../ja/secrets.md) | [Français](../fr/secrets.md)

设备数据绝不进入仓库、日志、默认导出或智能体可见的输出。本文档说明哪些数据属于机密、模拟器在运行时如何处理它们，以及 `cargo xtask secrets-check` 和 git hook 如何把它们挡在仓库之外。

## 1. 身份规则

本项目的任何文档、测试、golden、日志、示例、提交信息、issue 或 pull request 都不得出现设备的：

- MAC 地址(基础地址或派生地址)；
- 唯一 ID；
- eFuse 校准字；
- 备份文件名；
- cardid 内容。

示例和测试数据使用占位 MAC 前缀 `02:00:00`。设备序列号和守护进程令牌同样不得复制。需要讨论某个设备值时，只说明它的类型和位置(如 `calib_word` 位于 `file:0x1c`)，不写出值本身。

来自设备的文件保存在数据根目录下(macOS 为 `~/Library/Application Support/passportsim/`，Windows 为 `%LOCALAPPDATA%\passportsim\data\`)，其中设备数据放在仅属主可访问的目录中。

## 2. 哪些数据属于机密

| 数据 | 默认处理 | 显式启用 |
|---|---|---|
| 设备 flash 备份 | 除 `--flash <path>` 外从不加载，加载后机器被标记为受污染 | `--flash` 加 `--allow-tainted` |
| 原始 eFuse 转储(MAC、唯一 ID、校准值) | 从不读取；默认为 `--efuse synth` | `--efuse-dump <dir>`，会标记污染 |
| NVS 凭据(Wi-Fi、BLE 绑定密钥、应用令牌) | `inspect nvs` 显示命名空间、键和类型，从不显示凭据值；导出时擦除 NVS 页 | `inspect nvs --reveal`，需人工确认 |
| cardid 分区 `[0x356000, 0x35A000)` | 模拟器创建的镜像中为合成图案；从不打印、记录或导出；烧录规划器从不写入 | 无 |
| 输出中的 MAC、唯一 ID、校准字 | 合成时为占位值；受污染时被遮盖 | `--reveal identity`，需人工确认 |
| 真实 NFC 卡镜像 | `nfc.load` 时标记污染；`nfc.dump` 遮盖 UID 和 PWD/PACK | `--include-secrets` |
| 实时麦克风、桥接网络载荷、外部 HCI | 在本地记入日志以便回放，导出时丢弃 | `--include-secrets` |
| 守护进程令牌和启动码 | 位于运行时目录，仅属主可访问，从不出现在输出、日志或产物中 | 无 |
| 设备序列号、备份文件名 | 从不复制到仓库或文档中 | 无 |

**内置的 ROM ELF 不是机密。** `assets/rom/esp32c3_rev101_rom.elf` 和 `assets/rom/esp32c3_rev3_rom.elf` 是 Espressif 以 Apache-2.0 公开发布的 esp-rom-elfs，与其 [LICENSE](../../../assets/rom/LICENSE)、[NOTICE](../../../assets/rom/NOTICE) 以及记录 SHA-256 校验值的 [pins.toml](../../../assets/rom/pins.toml) 一起提交。发布包附带这三个文件。

**官方演示镜像从不进入仓库。** 只有当它的 SHA-256 与固定值一致、且 MIT 许可证文本存在时，`xtask package` 才会从构建主机的固件语料库中把它内嵌进去。

**受污染的加载只能由人执行。** 所有会污染机器的输入(对 cardid 字节不全为 0xFF 或含 NVS 凭据的镜像使用 `--flash`、`--efuse-dump`、对真实卡片使用 `nfc.load`)都是需要人工确认的原生命令行操作；MCP 和 HTTP 从不提供。

## 3. 机密集合

由一个纯函数 `pemu_api::secret_set` 决定什么算作身份信息。仓库扫描器和运行时遮盖都使用它，因此两者不会出现分歧。

成员，以及 `secrets-check` 以 `hashed:<kind>` 报告的类型：

| 类型 | 成员 | 形式 |
|---|---|---|
| `mac` | 基础 MAC 及 base+1 到 base+3(Wi-Fi station、soft-AP、BT、以太网) | 冒号、短横线和纯十六进制，大小写两种，以及字节反序；派生 MAC 同时包括 ESP-IDF 末字节回绕形式和 48 位进位形式 |
| `mac_suffix` | 每个 MAC 的 3 字节 NIC 后缀 | 仅文本形式 |
| `unique_id` | eFuse BLK2 唯一 ID(128 位) | 原始字节和纯十六进制，两种字节序 |
| `calib_word` | BLK2 中每个非零校准字 | 两种字节序的 4 个原始字节，以及十六进制文本 |
| `backup_stem` | 设备备份文件名的主干 | 主干的字节 |
| `cardid` | cardid 窗口中非 0xFF 的内容 | 按 32 字节分块哈希 |
| `nvs_credential` | 6 字节及以上的 NVS 凭据值 | 原始字节、十六进制和 base64；仅运行时 |
| `nfc_uid`、`nfc_pwd`、`nfc_pack` | NFC 标签的 UID、密码和密码确认 | 仅运行时 |
| `canary` | 第 5.6 节的随机 canary | 仅哈希文件 |

### 3.1 误报防护

为了让检查足够安静、值得信赖，构建器会丢弃短于 4 字节的成员(2 字节的 NFC PACK 只保留十六进制文本)、所有字节都相同的成员、非零字节少于 2 个的校准字，以及短于 6 字节的备份主干。派生 MAC 只加入 base+1 到 base+3。前 8000 字节中不含 NUL 字节的文件视为文本(git 的规则)；仅文本形式的成员只在文本文件中匹配。

## 4. 污染与遮盖

模拟器的运行时行为：

- **污染。** 由含机密的输入(第 2 节所列，或实时桥接)构建的机器是受污染的，它的分叉、快照和启动缓存条目同样受污染。受污染的机器：
  - 拒绝快照、flash 和产物的导出，报 `E_SECRET_REFUSED`，除非 `--include-secrets` 附带人工确认码；
  - 在回执中显示 `tainted: true`；
  - 启动缓存只保存在内存中；
  - 在原始内存工具(`mem_read`、`watch`、`trace`、`inspect heap`)中遮盖 cardid 窗口、NVS 页和任何与机密集合匹配的内容。
- **遮盖**作用于每一个智能体可见的文本和 JSON 输出，以及写入产物目录的每一个文件。它按值匹配：与机器机密集合匹配的内容变为 `<MAC>` 或 `<SECRET>`，智能体自己提供的地址保持不变。二进制产物(btsnoop、pcap、转储)以同样方式改写。
- **导出**会把 cardid 窗口填充为 0xFF，擦除 NVS 分区，省略 eFuse 字节，并丢弃实时日志载荷。遮盖后的快照以出厂 NVS 和合成的 cardid 启动，并在回执中注明。
- **日志。** 没有遥测。守护进程日志经过遮盖。崩溃报告不含 RAM 或 flash 内容，除非用户要求。
- **网络。** 中继和桥接默认拒绝回环、私有、链路本地和 ULA 地址段，并记录每一个目标地址。

## 5. 仓库卫生

经过规范化和身份遮盖的文本 golden 可以提交；检查仍会扫描它们。

### 5.1 `.gitignore`

[`.gitignore`](../../../.gitignore) 忽略固件和设备数据(`*.bin`、`*.elf`、`efuse_blk*`、`*flash*.bin`、`cardid*`、`boot_log*`、`GROUND_TRUTH*`)、运行产物(`*.snap`、`*.pebundle`、`*.pcap`、`*.btsnoop`、`*.wav`、`/artifacts/`、`.passportsim/`)和本地配置(`*.local.toml`、`secrets-check.toml`)。`!` 条目只重新包含两个内置 ROM ELF 和少数经过审查的测试 ELF。`git add -f` 可以绕过 `.gitignore`，所以真正的防线是 `xtask secrets-check`。

### 5.2 模式规则

模式规则不需要本地数据(`xtask/src/secrets/`)：

| 规则 | 拒绝 |
|---|---|
| `mac-shape` | MAC 形状的文本(六组十六进制数，以 `:` 或 `-` 连接)，`02:00:00` 前缀、全零地址和组地址除外 |
| `efuse-dump` | 大小和结构与原始 eFuse 块转储相符的二进制文件(第 6.1 节) |
| `nvs-credential` | 含凭据键的 NVS 分区 |
| `cardid-window` | 任何足够长的二进制文件中，cardid 窗口 `[0x356000, 0x35A000)` 内的非 0xFF 字节 |
| `backup-name` | 符合设备备份命名模式的文件名(第 6.2 节) |
| `rom-pin` | `assets/rom/` 下任何不是已在 `assets/rom/pins.toml` 中固定的 ELF 的二进制文件，或缺少 `LICENSE` 或 `NOTICE` 时该目录下的任何二进制文件 |

已固定的 ROM ELF 不受内容规则约束。打包时，`cardid-window` 还会跳过刚构建出的 `passportsim` 可执行文件和 `pemu_wasm.wasm`，但前提是文件在结构上是有效的可执行文件(PE、ELF、Mach-O 或 wasm)：两者都大于 0x35A000 字节，该偏移处是程序代码。在 flash 镜像前伪造文件头仍会被拒绝。

### 5.3 哈希规则

哈希规则用于捕获模式规则漏掉的设备值形式(纯十六进制、字节数组、反序字节、校准字、cardid 分块)。`xtask secrets-check --init` 用随机盐对机密集合的每个成员做哈希，写入 `~/.config/passportsim/secrets-check.toml`(Windows 为 `%APPDATA%\passportsim\secrets-check.toml`)，仅属主可访问(macOS 上为 0700 目录中的 0600 文件，Windows 上为受保护的 DACL，每次加载时重新检查)。该文件：

- 绝不进入仓库，也绝不离开保存设备数据的主机；
- 只保存加盐的 SHA-256 哈希，以及随机 canary；
- 智能体不得读取、打印或复制它。

**输出规则。** `secrets-check` 从不打印匹配到的内容、成员值或哈希，只输出规则名、`file:offset` 和计数。触发 `backup-name` 的文件在报告中会隐去文件名。

### 5.4 各类规则的运行位置

模式规则在所有主机上运行。哈希规则在存在设备目录的主机上运行；每台这样的主机用 `--init` 生成自己的哈希文件，并且必须在该主机上进行真机烧录之前完成。

| 位置 | 模式规则 | 哈希规则 |
|---|---|---|
| `cargo xtask secrets-check`(整个工作树或 `--paths`) | 是 | 哈希文件存在时运行，否则跳过并给出提示 |
| pre-commit hook(`--staged`)和 pre-push hook(`--hook pre-push`) | 是 | 是；失败即拒绝 |
| 有设备目录的主机上的 T0 | 是 | 是 |
| 没有设备目录的主机上的 T0 | 是 | 否；回执中注明 "pattern rules only" |
| T1 | 是 | 是 |

检查只从已知文件夹(Windows)或 `HOME`(macOS)查找文件；设置了 `PASSPORTSIM_HOME`、`PASSPORTSIM_CONFIG_DIR` 或 `PASSPORTSIM_DATA_ROOT` 时，除非给出 `--root`，否则拒绝运行，以免这些覆盖设置把设备目录藏起来。

失败即拒绝：

- 在有设备目录但没有哈希文件的机器上，hook 会拒绝，并提示运行 `cargo xtask secrets-check --init`；
- 所有模式都拒绝无法读取、格式错误或不是仅属主可访问的哈希文件；
- hook 通过 `exec cargo xtask ...` 运行，因此缺少 cargo 或构建失败时会拒绝，而不是跳过检查。

### 5.5 命令

```text
cargo xtask secrets-check [--root <dir>]                  scan git ls-files -co --exclude-standard
cargo xtask secrets-check [--root <dir>] --paths <files>  scan the given files
cargo xtask secrets-check [--root <dir>] --staged         what the pre-commit hook runs
cargo xtask secrets-check [--root <dir>] --hook pre-push  what the pre-push hook runs
cargo xtask hooks install [--root <dir>] [--force]        install both hooks (maintainer only)
cargo xtask secrets-check --init                          write the hash file (maintainer only)
cargo xtask secrets-check --self-test                     check the hash file (maintainer only)
```

智能体只能运行前四条。使用 `--paths` 时，相对路径在给出 `--root` 时相对于它解析，否则相对于当前目录。

### 5.6 启用检查

在新的克隆中、首次提交之前，维护者：

1. 运行 `cargo xtask secrets-check --init`，它从设备目录构建机密集合，并用新的盐和随机 canary 写入哈希文件(任何读取失败时都不写入，因此不会只哈希部分集合)；
2. 运行 `cargo xtask hooks install`；
3. 运行 `cargo xtask secrets-check --self-test`，确认扫描器能检测出每一种成员形式，只输出计数；
4. 暂存一个包含 canary 的临时文件，确认 `git commit` 被拒绝，然后取消暂存并删除它。

各 worktree 共享 hook。重新运行 `--init` 会替换盐和 canary，因此之后要重做第 4 步。

### 5.7 提交被拒绝时

hook 会为每个命中打印一行(规则、文件、偏移)。然后：

1. **不要绕过检查。** 不要使用 `git commit --no-verify`，不要修改 `core.hooksPath`，不要编辑或删除 hook，不要碰哈希文件。
2. **修正内容，而不是检查。**
   - 文档或测试中的 `mac-shape`：改用 `02:00:00` 占位地址。
   - 测试数据上的 `efuse-dump` 或 `backup-name`：按第 6 节处理。
   - `cardid-window` 或 `nvs-credential`：该二进制文件不应进入仓库；在测试时生成，或放在数据根目录下。
   - `rom-pin`：`assets/rom/` 中只能放已固定的 ROM ELF。
   - `hashed:<kind>`：有真实设备值进入了文件。删除它，并查明来源。
3. **报告时不写出值。** 只写规则名和 `file:offset`，绝不引用匹配到的字节。
4. **与哈希文件有关的拒绝**(缺失、无法读取、权限错误)交给维护者处理。不要自己运行 `--init`。
5. **疑似误报**连同规则名和 `file:offset` 交给维护者。按设计没有允许列表；应通过文件名或测试数据格式解决。

## 6. 测试数据规则

两条规则都没有允许列表：允许列表就是真实转储可以溜过的后门。

### 6.1 小型二进制文件的大小规则

`efuse-dump` 拒绝：

- 任何 24 字节或 32 字节(一个原始 eFuse 块的大小)、不是均匀填充、也没有容器魔数(ELF、PNG、GIF、JPEG、gzip、zip、zstd、wasm、PDF)的二进制文件，包括以 `.bin` 保存的原始 32 字节 SHA-256 摘要；
- 没有容器魔数、且 BLK1 MAC 字段为非零单播地址的 336 字节二进制文件(全部 11 个块)。

这些大小的测试数据应在测试时生成，或使用容器格式；摘要以十六进制文本保存。全 0x00 或全 0xFF 的均匀填充没有问题。

### 6.2 合成镜像的命名规则

`backup-name` 在以下情况下标记路径(不区分大小写)：

- 某个目录名为 `passport-backups`；
- 文件名以 `efuse_blk`、`cardid`、`boot_log` 或 `ground_truth` 开头；
- 文件是原始转储(`.bin`、`.img`、`.dump`、`.dmp` 或 `.raw`，其后可再跟 `.gz`、`.xz`、`.zst`、`.bz2`、`.zip` 或 `.7z`)，且主干包含 `backup`、`dump`、`flash`、`full`、`efuse`、`nvs`、`cardid`、`readback`、`passport`、`4m`、`8m` 或 `16m`；
- 任何路径组成部分带有 MAC：带分隔符的 MAC 形状，或至少含一个数字和一个字母的 12 位十六进制串(占位地址、全零地址和组地址除外)。

合成镜像的命名应避开这些词，例如 `synthetic_image.bin` 或 `seeded_card_erased.img`。`.gitignore` 也忽略这些词，所以二进制测试数据还需要在那里添加一条经过审查的 `!` 条目。确切的备份主干由哈希规则按值匹配。

## 7. hook 的扫描范围

- **pre-commit** 扫描暂存的 blob，而不是工作树。修复后要重新暂存文件。
- **pre-push** 扫描 `remote..local` 中的每个 blob。对于新的或未知的远端提交，它扫描推送的整个树，以及不在任何远端跟踪引用上的提交中的 blob，因此历史不会通过新分支泄漏。手动运行 `--hook pre-push` 时扫描 `@{upstream}..HEAD`，没有上游时扫描 `HEAD` 树。
- **安装。** hook 安装到 `git rev-parse --git-path hooks`，因此各 worktree 共享。已有的、不带 xtask 标记的 hook 会保留，除非给出 `--force`。脚本是 `sh` 脚本，在两种主机上都由 Git 自带的 shell 运行。
- **`--init` 的输入。** 设备目录取本地配置中的 `[paths] data_root`，否则取默认数据根目录。备份是备份目录中直接存放的 `.bin` 文件。
- **测试。** `xtask` 的测试构建会拒绝设备和 hook 模式，并在空的 `HOME` 下扫描，因此任何测试都无法读取设备数据或真正的哈希文件。

## 8. 设备安全

**谁可以打开设备。**

- 模拟器、守护进程及其端点、CI、测试和智能体从不打开 `/dev/cu.*`、`/dev/tty.*` 或 `COM` 端口，也从不运行 `esptool`、`espefuse`、`idf.py flash` 或 `idf.py monitor`。
- 只有启用 `device` 特性的 `pemu-planner` 会打开端口，而且只在人工确认之后(`docs/ARCHITECTURE.md`，“Flashing a real device”一节)。发现设备时按 VID/PID 枚举，不打开端口，因为 esptool 默认的复位会让应用进入下载模式。烧录真实设备只能通过原生命令行，不能通过浏览器。

**开发用采集**(golden、探针运行、校准)在真实设备上进行时：

1. 先备份；
2. 只写入引导程序、分区表和应用段；
3. 绝不写入 cardid 范围 0x356000 到 0x359FFF；
4. 绝不整片擦除 flash，绝不烧写 eFuse；
5. 完成后从备份中恢复 Passport Keys，并比对 cardid 的 MD5。

设备数据的其他任何用途(例如 `--efuse-dump`)都需要设备所有者的明确批准。

**人工确认**用于阻止协作中的智能体出现错误的工具调用。它无法阻止以同一用户身份拥有 shell 的恶意智能体，后者可以直接运行 esptool；下面的防护措施针对这种情况。确认途径依次为：

1. MCP elicitation，由用户在客户端中回答；
2. 守护进程弹出的原生对话框(在桌面会话中)；
3. 控制终端上的一次性代码，仅适用于在交互式控制台中前台运行的命令。该代码从不显示在网页界面中，不会在工具结果中返回，也不会写入文件。

同样的途径也用于 `inspect nvs --reveal`、`--reveal identity`、`--include-secrets` 和受污染的加载。

**随附的防护措施。** [智能体技能说明](../../../skills/passportsim/SKILL.md)和 [`device-deny.json`](../../../skills/passportsim/device-deny.json)(用于 Claude Code `.claude/settings.json` 的 `permissions.deny` 列表)会拒绝那些能访问真实设备的常见写法：`esptool`、`esptool.py`、`python -m esptool`、`py -m esptool`、虚拟环境中的 `python.exe` 路径、`esptool.exe`、`espefuse`、`espefuse.exe`、`idf.py flash`、`idf.py -p /dev/cu.*`、`idf.py -p COM*` 以及 `\\.\COM*` 路径，并把烧录引导到烧录规划器。这份列表只能尽力而为，因为模式列表无法覆盖所有 shell 和引号写法。**真正强制执行的允许列表位于解析参数的地方**：规划器和技能自己的工具只接受 `socket://127.0.0.1:*` 和 `rfc2217://127.0.0.1:*` 作为端口，并且在启动任何进程之前就会检查。

本文档随技能说明一起包含在每个发布包中。在发布包中，指向 `.gitignore` 的链接无法打开，因为发布包不含仓库文件。
