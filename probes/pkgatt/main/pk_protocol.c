// main/pk_protocol.c: Passport Keys 协议的纯逻辑实现。
#include "pk_protocol.h"

#include <inttypes.h>
#include <stdio.h>
#include <string.h>

static const char *const KEY_NAMES[PK_KEY_COUNT] = { "up", "down", "ok" };

const char *pk_key_name(pk_key_t key)
{
    return (unsigned)key < PK_KEY_COUNT ? KEY_NAMES[key] : "?";
}

// snprintf 截断时返回值 >= cap;统一视为失败,避免发出被截断的半行 JSON。
static size_t finish(int n, size_t cap)
{
    return (n > 0 && (size_t)n < cap) ? (size_t)n : 0;
}

size_t pk_format_hello(char *buf, size_t cap, const char *version, uint32_t boot_id)
{
    return finish(snprintf(buf, cap,
                           "{\"t\":\"hello\",\"fw\":\"" PK_FIRMWARE_NAME "\",\"proto\":%d,"
                           "\"ver\":\"%s\",\"boot\":\"%08" PRIx32 "\"}\n",
                           PK_PROTO_VERSION, version, boot_id),
                  cap);
}

size_t pk_format_button(char *buf, size_t cap, pk_key_t key, uint32_t seq, uint32_t boot_id)
{
    if ((unsigned)key >= PK_KEY_COUNT) return 0;
    return finish(snprintf(buf, cap,
                           "{\"t\":\"btn\",\"k\":\"%s\",\"e\":\"press\",\"seq\":%" PRIu32
                           ",\"boot\":\"%08" PRIx32 "\"}\n",
                           KEY_NAMES[key], seq, boot_id),
                  cap);
}

size_t pk_format_battery(char *buf, size_t cap, int soc, int mv)
{
    return finish(snprintf(buf, cap, "{\"t\":\"bat\",\"soc\":%d,\"mv\":%d}\n", soc, mv), cap);
}

size_t pk_format_pong(char *buf, size_t cap)
{
    return finish(snprintf(buf, cap, "{\"t\":\"pong\"}\n"), cap);
}

size_t pk_format_ack(char *buf, size_t cap, const char *cmd)
{
    return finish(snprintf(buf, cap, "{\"t\":\"ack\",\"cmd\":\"%s\"}\n", cmd), cap);
}

// ---------------------------------------------------------------------------
// 命令解析:一个只覆盖本协议需要的 JSON 子集的小解析器。
// 不引入 cJSON:命令只有扁平的字符串字段与一个整数字段,手写解析可以固定栈内存、不做堆分配。
// ---------------------------------------------------------------------------

typedef struct {
    const char *p;
    const char *end;
} cursor_t;

static void skip_ws(cursor_t *c)
{
    while (c->p < c->end && (*c->p == ' ' || *c->p == '\t' || *c->p == '\r' || *c->p == '\n')) {
        c->p++;
    }
}

static int hex_value(char ch)
{
    if (ch >= '0' && ch <= '9') return ch - '0';
    if (ch >= 'a' && ch <= 'f') return ch - 'a' + 10;
    if (ch >= 'A' && ch <= 'F') return ch - 'A' + 10;
    return -1;
}

static bool read_hex4(cursor_t *c, unsigned *out)
{
    if (c->end - c->p < 4) return false;
    unsigned value = 0;
    for (int i = 0; i < 4; i++) {
        int h = hex_value(c->p[i]);
        if (h < 0) return false;
        value = value * 16U + (unsigned)h;
    }
    c->p += 4;
    *out = value;
    return true;
}

// 读取 JSON 字符串到 out(容量 cap,总以 NUL 结尾)。
// 非 ASCII 内容(UTF-8 多字节序列、\u 转义 >= 0x80、代理对)各折叠为一个 '?'。
static bool read_string(cursor_t *c, char *out, size_t cap)
{
    if (c->p >= c->end || *c->p != '"' || cap == 0) return false;
    c->p++;

    size_t n = 0;
    while (c->p < c->end) {
        unsigned char ch = (unsigned char)*c->p++;
        int value;

        if (ch == '"') {
            out[n] = '\0';
            return true;
        }
        if (ch == '\\') {
            if (c->p >= c->end) return false;
            char esc = *c->p++;
            switch (esc) {
            case '"':  value = '"';  break;
            case '\\': value = '\\'; break;
            case '/':  value = '/';  break;
            // 控制字符在屏幕上没有意义,显示为空格。
            case 'b': case 'f': case 'n': case 'r': case 't':
                value = ' ';
                break;
            case 'u': {
                unsigned code;
                if (!read_hex4(c, &code)) return false;
                // 高代理后紧跟低代理时一并吞掉,整个字符只输出一个 '?'。
                if (code >= 0xD800 && code <= 0xDBFF && c->end - c->p >= 6 &&
                    c->p[0] == '\\' && c->p[1] == 'u') {
                    cursor_t peek = { c->p + 2, c->end };
                    unsigned low;
                    if (read_hex4(&peek, &low) && low >= 0xDC00 && low <= 0xDFFF) {
                        c->p = peek.p;
                    }
                }
                value = (code >= 0x20 && code < 0x7F) ? (int)code : '?';
                break;
            }
            default:
                return false;
            }
        } else if (ch < 0x20) {
            return false;                   // JSON 不允许未转义的控制字符
        } else if (ch >= 0x80) {
            if ((ch & 0xC0) == 0x80) continue;  // UTF-8 续字节:首字节已输出 '?'
            value = '?';
        } else {
            value = ch;
        }

        if (n + 1 < cap) out[n++] = (char)value;
    }
    return false;                           // 字符串未闭合
}

// 跳过数字、true/false/null 等标量。命令对象是扁平的,嵌套对象或数组直接判为非法。
static bool skip_scalar(cursor_t *c)
{
    const char *start = c->p;
    while (c->p < c->end) {
        char ch = *c->p;
        if (ch == ',' || ch == '}' || ch == ' ' || ch == '\t' || ch == '\r' || ch == '\n') break;
        if (ch == '{' || ch == '[' || ch == '"' || ch == ':') return false;
        c->p++;
    }
    return c->p > start;
}

// 读取 JSON 非负整数:不接受负号和前导零,超过 max 视为非法。
// 数字后面的 '.'、'e' 会被调用方的分隔符检查拒绝,因此小数和指数写法也无效。
static bool read_uint(cursor_t *c, uint32_t max, uint32_t *out)
{
    const char *start = c->p;
    uint64_t value = 0;
    while (c->p < c->end && *c->p >= '0' && *c->p <= '9') {
        value = value * 10U + (uint64_t)(*c->p - '0');
        if (value > max) return false;
        c->p++;
    }
    size_t digits = (size_t)(c->p - start);
    if (digits == 0 || (digits > 1 && *start == '0')) return false;
    *out = (uint32_t)value;
    return true;
}

bool pk_parse_command(const char *line, size_t len, pk_cmd_t *out)
{
    memset(out, 0, sizeof(*out));
    cursor_t c = { line, line + len };
    char cmd[PK_LABEL_MAX] = "";

    skip_ws(&c);
    if (c.p >= c.end || *c.p != '{') return false;
    c.p++;

    for (;;) {
        char key[16];
        char value[PK_LABEL_MAX];

        skip_ws(&c);
        if (!read_string(&c, key, sizeof(key))) return false;
        skip_ws(&c);
        if (c.p >= c.end || *c.p != ':') return false;
        c.p++;
        skip_ws(&c);

        if (c.p < c.end && *c.p == '"') {
            if (!read_string(&c, value, sizeof(value))) return false;
            if (strcmp(key, "cmd") == 0) {
                memcpy(cmd, value, sizeof(cmd));   // read_string 保证 value 以 NUL 结尾
            } else {
                for (int i = 0; i < PK_KEY_COUNT; i++) {
                    if (strcmp(key, KEY_NAMES[i]) == 0) {
                        memcpy(out->label[i], value, sizeof(value));
                        out->has_label[i] = true;
                    }
                }
            }
        } else if (strcmp(key, "screen_off") == 0) {
            if (!read_uint(&c, PK_SCREEN_OFF_MAX_S, &out->screen_off_s)) return false;
            out->has_screen_off = true;
        } else if (!skip_scalar(&c)) {
            return false;
        }

        skip_ws(&c);
        if (c.p >= c.end) return false;
        if (*c.p == ',') {
            c.p++;
            continue;
        }
        if (*c.p != '}') return false;
        c.p++;
        break;
    }

    skip_ws(&c);
    if (c.p != c.end) return false;

    if (strcmp(cmd, "hello") == 0) {
        out->type = PK_CMD_HELLO;
    } else if (strcmp(cmd, "ping") == 0) {
        out->type = PK_CMD_PING;
    } else if (strcmp(cmd, "labels") == 0) {
        out->type = PK_CMD_LABELS;
    } else if (strcmp(cmd, "bye") == 0) {
        out->type = PK_CMD_BYE;
    } else if (strcmp(cmd, "config") == 0) {
        if (!out->has_screen_off) return false;
        out->type = PK_CMD_CONFIG;
    } else {
        return false;
    }
    return true;
}

// ---------------------------------------------------------------------------
// 行组帧
// ---------------------------------------------------------------------------

void pk_line_init(pk_line_t *line)
{
    line->len = 0;
    line->discarding = false;
}

void pk_line_feed(pk_line_t *line, const uint8_t *data, size_t len, pk_line_cb_t cb, void *user)
{
    for (size_t i = 0; i < len; i++) {
        uint8_t b = data[i];
        if (b == '\n') {
            if (!line->discarding) {
                if (line->len > 0 && line->buf[line->len - 1] == '\r') line->len--;
                if (line->len > 0) {
                    line->buf[line->len] = '\0';
                    cb(line->buf, line->len, user);
                }
            }
            line->len = 0;
            line->discarding = false;
        } else if (line->discarding) {
            continue;
        } else if (line->len + 1 >= sizeof(line->buf)) {
            // 保留一个字节放 NUL;超长行整行丢弃,不把残片当成命令解析。
            line->len = 0;
            line->discarding = true;
        } else {
            line->buf[line->len++] = (char)b;
        }
    }
}
