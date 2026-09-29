// main/pk_protocol.h: Passport Keys 与 macOS app 之间的 JSON Lines 协议(v1)。
// 纯 C 实现,不依赖 ESP-IDF/LVGL,由 tests/test_pk_protocol.c 在主机上测试。
// 字段定义与 Mac 端 PassportProtocol.swift 保持一致,见仓库外层 docs/protocol.md。
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define PK_FIRMWARE_NAME  "passport-keys"
#define PK_PROTO_VERSION  1

// 单行上限(含 NUL)。labels 命令带三个 31 字节标签时约 140 字节,留足余量。
#define PK_LINE_MAX       256
// 单个标签上限(含 NUL)。屏幕一行约显示 16 个字符,超出部分由 UI 省略。
#define PK_LABEL_MAX      32
// 设备发出的单条消息上限(含 NUL)。
#define PK_MSG_MAX        128
// config 命令里 screen_off 的上限(秒,即 24 小时);0 表示永不熄屏。
#define PK_SCREEN_OFF_MAX_S 86400

typedef enum {
    PK_KEY_UP = 0,
    PK_KEY_DOWN,
    PK_KEY_OK,
    PK_KEY_COUNT,
} pk_key_t;

typedef enum {
    PK_CMD_HELLO = 0,
    PK_CMD_PING,
    PK_CMD_LABELS,
    PK_CMD_BYE,
    PK_CMD_CONFIG,
} pk_cmd_type_t;

typedef struct {
    pk_cmd_type_t type;
    bool has_label[PK_KEY_COUNT];
    char label[PK_KEY_COUNT][PK_LABEL_MAX];
    bool has_screen_off;
    uint32_t screen_off_s;      // config:空闲多少秒后熄屏,0 表示永不熄屏
} pk_cmd_t;

// 协议里的键名:"up" / "down" / "ok";越界返回 "?"。
const char *pk_key_name(pk_key_t key);

// 以下函数写入一行以 '\n' 结尾、NUL 终止的 JSON,返回不含 NUL 的长度。
// 缓冲区放不下时返回 0,调用方应丢弃该帧,不能发出半行 JSON。
size_t pk_format_hello(char *buf, size_t cap, const char *version, uint32_t boot_id);
size_t pk_format_button(char *buf, size_t cap, pk_key_t key, uint32_t seq, uint32_t boot_id);
size_t pk_format_battery(char *buf, size_t cap, int soc, int mv);
size_t pk_format_pong(char *buf, size_t cap);
size_t pk_format_ack(char *buf, size_t cap, const char *cmd);

// 解析一行 host 命令(不含 '\n')。只接受扁平 JSON 对象:未知命令、嵌套值、语法错误都返回 false。
// 标签里的非 ASCII 字符折叠为单个 '?',超长部分截断,因为屏幕字体只覆盖 ASCII。
// config 命令必须带 0..PK_SCREEN_OFF_MAX_S 的整数 screen_off,否则整条命令无效。
bool pk_parse_command(const char *line, size_t len, pk_cmd_t *out);

// 按 '\n' 组帧。USB 读取和 BLE 写入都可能把一行拆成多段,每条链路各持有一个实例。
typedef struct {
    char buf[PK_LINE_MAX];
    size_t len;
    bool discarding;        // 当前行已超长,丢弃到下一个 '\n'
} pk_line_t;

// 每收到完整一行回调一次;line 以 NUL 结尾,不含 '\r\n',空行不回调。
typedef void (*pk_line_cb_t)(const char *line, size_t len, void *user);

void pk_line_init(pk_line_t *line);
void pk_line_feed(pk_line_t *line, const uint8_t *data, size_t len, pk_line_cb_t cb, void *user);
