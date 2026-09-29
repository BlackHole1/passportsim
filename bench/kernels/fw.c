/* Firmware-like, call-heavy RV32IMC kernel for the rv32-interp spike.
 * Mimics the control-flow shape of IDF/LVGL firmware: short basic blocks,
 * many small non-inlined calls and returns, indirect calls through class
 * tables (LVGL draw/event callbacks), queue send/receive wrapped in
 * critical-section helpers (FreeRTOS), sorted timer-list insertion
 * (esp_timer), and a printf-style formatter (ESP_LOG).
 * Entry: bench_main(iterations) returns a checksum. No pointer values are
 * hashed, so host and RV32 builds agree.
 */
#include <stdint.h>

#define NOBJ 40
#define NTYPES 4
#define FBW 64
#define FBH 48
#define QLEN 16
#define NTMR 24

#define NI __attribute__((noinline))

typedef struct { int16_t x1, y1, x2, y2; } area_t;
struct obj;
typedef void (*draw_fn)(struct obj *o, const area_t *clip);
typedef uint32_t (*event_fn)(struct obj *o, uint32_t code);
typedef struct obj { area_t coords; uint16_t color; uint8_t type; uint8_t flags; uint32_t state; } obj_t;
typedef struct { draw_fn draw; event_fn event; } class_t;

static obj_t objs[NOBJ];
static uint16_t fb2[FBW * FBH];
static uint32_t rng = 0x12345678u;
static uint32_t crit_nesting;

NI static uint32_t next_rand(void) {
    rng ^= rng << 13;
    rng ^= rng >> 17;
    rng ^= rng << 5;
    return rng;
}

NI static int16_t max16(int16_t a, int16_t b) { return a > b ? a : b; }
NI static int16_t min16(int16_t a, int16_t b) { return a < b ? a : b; }

NI static int area_intersect(area_t *res, const area_t *a, const area_t *b) {
    res->x1 = max16(a->x1, b->x1);
    res->y1 = max16(a->y1, b->y1);
    res->x2 = min16(a->x2, b->x2);
    res->y2 = min16(a->y2, b->y2);
    return res->x1 <= res->x2 && res->y1 <= res->y2;
}

NI static uint16_t color_mix(uint16_t c1, uint16_t c2, uint8_t mix) {
    uint32_t r = (((c1 >> 11) & 31u) * mix + ((c2 >> 11) & 31u) * (255u - mix)) >> 8;
    uint32_t g = (((c1 >> 5) & 63u) * mix + ((c2 >> 5) & 63u) * (255u - mix)) >> 8;
    uint32_t b = ((c1 & 31u) * mix + (c2 & 31u) * (255u - mix)) >> 8;
    return (uint16_t)((r << 11) | (g << 5) | b);
}

NI static void put_px(int x, int y, uint16_t color, uint8_t opa) {
    uint16_t *d = &fb2[y * FBW + x];
    *d = opa >= 250 ? color : color_mix(color, *d, opa);
}

NI static void fill_area(const area_t *a, uint16_t color, uint8_t opa) {
    for (int y = a->y1; y <= a->y2; y++)
        for (int x = a->x1; x <= a->x2; x++) put_px(x, y, color, opa);
}

static void draw_rect(obj_t *o, const area_t *clip) {
    area_t a;
    if (area_intersect(&a, &o->coords, clip)) fill_area(&a, o->color, 255);
}

static void draw_label(obj_t *o, const area_t *clip) {
    area_t a;
    if (!area_intersect(&a, &o->coords, clip)) return;
    for (int i = 0; i < 4; i++) {
        area_t g = {(int16_t)(a.x1 + i * 3), a.y1, (int16_t)(a.x1 + i * 3 + 1), (int16_t)(a.y1 + 2)};
        area_t gg;
        if (area_intersect(&gg, &g, clip)) fill_area(&gg, (uint16_t)(o->color ^ 0xffff), 128);
    }
}

static void draw_button(obj_t *o, const area_t *clip) {
    draw_rect(o, clip);
    area_t inner = {(int16_t)(o->coords.x1 + 1), (int16_t)(o->coords.y1 + 1), (int16_t)(o->coords.x2 - 1),
                    (int16_t)(o->coords.y2 - 1)};
    area_t r;
    if (area_intersect(&r, &inner, clip)) fill_area(&r, color_mix(o->color, 0xffff, 200), 180);
}

static void draw_img(obj_t *o, const area_t *clip) {
    area_t a;
    if (!area_intersect(&a, &o->coords, clip)) return;
    for (int y = a.y1; y <= a.y2; y += 2)
        for (int x = a.x1; x <= a.x2; x += 2) put_px(x, y, (uint16_t)(o->color + (uint16_t)(x * y)), 255);
}

static uint32_t ev_default(obj_t *o, uint32_t code) {
    o->state ^= code;
    return o->state & 7u;
}

static uint32_t ev_button(obj_t *o, uint32_t code) {
    if (code & 1u) o->state++;
    return ev_default(o, code >> 1) + 1u;
}

static const class_t classes[NTYPES] = {
    {draw_rect, ev_default}, {draw_label, ev_default}, {draw_button, ev_button}, {draw_img, ev_default}};

typedef struct { uint32_t buf[QLEN]; uint8_t head, tail, count; } queue_t;
static queue_t q;

NI static void enter_critical(void) { crit_nesting++; }
NI static void exit_critical(void) { crit_nesting--; }

NI static int queue_send(queue_t *qq, uint32_t v) {
    enter_critical();
    if (qq->count == QLEN) {
        exit_critical();
        return 0;
    }
    qq->buf[qq->tail] = v;
    qq->tail = (uint8_t)((qq->tail + 1) % QLEN);
    qq->count++;
    exit_critical();
    return 1;
}

NI static int queue_recv(queue_t *qq, uint32_t *v) {
    enter_critical();
    if (qq->count == 0) {
        exit_critical();
        return 0;
    }
    *v = qq->buf[qq->head];
    qq->head = (uint8_t)((qq->head + 1) % QLEN);
    qq->count--;
    exit_critical();
    return 1;
}

typedef struct { uint32_t expiry; uint8_t next; } tmr_t;
static tmr_t tmrs[NTMR];
static uint8_t tmr_head = 0xff;

NI static void tmr_insert(uint8_t id, uint32_t expiry) {
    tmrs[id].expiry = expiry;
    uint8_t *link = &tmr_head;
    while (*link != 0xff && tmrs[*link].expiry <= expiry) link = &tmrs[*link].next;
    tmrs[id].next = *link;
    *link = id;
}

NI static int tmr_pop(void) {
    if (tmr_head == 0xff) return -1;
    uint8_t id = tmr_head;
    tmr_head = tmrs[id].next;
    return id;
}

NI static int fmt_u32(char *out, uint32_t v, uint32_t base) {
    char tmp[12];
    int n = 0;
    do {
        uint32_t d = v % base;
        tmp[n++] = (char)(d < 10 ? '0' + d : 'a' + d - 10);
        v /= base;
    } while (v);
    for (int i = 0; i < n; i++) out[i] = tmp[n - 1 - i];
    return n;
}

NI static int mini_printf(char *out, const char *fmt, uint32_t a, uint32_t b) {
    int n = 0;
    uint32_t args[2] = {a, b};
    int ai = 0;
    for (const char *p = fmt; *p; p++) {
        if (*p != '%') {
            out[n++] = *p;
            continue;
        }
        p++;
        switch (*p) {
        case 'u': n += fmt_u32(out + n, args[ai++ & 1], 10); break;
        case 'x': n += fmt_u32(out + n, args[ai++ & 1], 16); break;
        case 'c': out[n++] = (char)(args[ai++ & 1] & 0x7f); break;
        case '%': out[n++] = '%'; break;
        default: out[n++] = '?'; break;
        }
    }
    out[n] = 0;
    return n;
}

NI static uint32_t hash_bytes(const char *s, int n, uint32_t h) {
    for (int i = 0; i < n; i++) h = (h ^ (uint8_t)s[i]) * 16777619u;
    return h;
}

uint32_t bench_main(uint32_t iterations) {
    for (int i = 0; i < NOBJ; i++) {
        int16_t x = (int16_t)(next_rand() % (FBW - 8));
        int16_t y = (int16_t)(next_rand() % (FBH - 8));
        objs[i].coords = (area_t){x, y, (int16_t)(x + 2 + next_rand() % 10), (int16_t)(y + 2 + next_rand() % 8)};
        if (objs[i].coords.x2 >= FBW) objs[i].coords.x2 = FBW - 1;
        if (objs[i].coords.y2 >= FBH) objs[i].coords.y2 = FBH - 1;
        objs[i].color = (uint16_t)next_rand();
        objs[i].type = (uint8_t)(next_rand() % NTYPES);
    }
    static const char *const fmts[4] = {"I (%u) main: tick %x\n", "W (%u) disp: flush %u px\n", "key=%c val=%u%%\n",
                                        "E (%x) i2c: nack %u\n"};
    char line[64];
    uint32_t check = 0;
    for (uint32_t it = 0; it < iterations; it++) {
        int16_t cx = (int16_t)(next_rand() % (FBW / 2));
        int16_t cy = (int16_t)(next_rand() % (FBH / 2));
        area_t clip = {cx, cy, (int16_t)(cx + FBW / 2), (int16_t)(cy + FBH / 2)};
        for (int i = 0; i < NOBJ; i++) classes[objs[i].type].draw(&objs[i], &clip);

        for (int i = 0; i < 24; i++) queue_send(&q, next_rand());
        uint32_t v;
        while (queue_recv(&q, &v)) {
            obj_t *o = &objs[v % NOBJ];
            check += classes[o->type].event(o, v >> 8);
        }

        for (int i = 0; i < NTMR; i++) tmr_insert((uint8_t)i, next_rand() % 5000u);
        int id, prev = -1;
        while ((id = tmr_pop()) >= 0) {
            check = check * 3u + (uint32_t)id + (prev >= 0 ? tmrs[prev].expiry : 0u);
            prev = id;
        }

        for (int i = 0; i < 6; i++) {
            int n = mini_printf(line, fmts[(it + i) & 3], it * 7u + (uint32_t)i, check);
            check = hash_bytes(line, n, check);
        }
        for (int i = 0; i < FBW * FBH; i += 37) check = check * 31u + fb2[i];
        check ^= crit_nesting;
    }
    return check;
}

#ifdef HOST
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    uint32_t it = argc > 1 ? (uint32_t)strtoul(argv[1], 0, 0) : 10;
    printf("iterations=%u checksum=0x%08x\n", it, bench_main(it));
    return 0;
}
#endif
