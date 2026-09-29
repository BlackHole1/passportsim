// probe_boot_facts: the boot-time identity and layout facts.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   CHIP    esp_chip_info model, revision, cores and feature bits
//   EFUSE   wafer major/minor, block major/minor, package version, ADC calibration version
//   RESET   esp_reset_reason and the RTC_CNTL reset-cause register value
//   STRAP   GPIO_STRAP_REG; the board value is 0x0A, measured on the device
//   FLASH   JEDEC id and size; the device part answers 0x204017, 8 MB
//   HEAPREG one line per heap region the heap component knows, start/end/size and caps
//   HEAP    totals per capability set
//   MAC     whether the base MAC is the placeholder 02:00:00:..
//   PART    the partition table as the app sees it
//
// Compared against the device boot log facts and the QEMU console. The ECO7 heap-region
// expectation (`3FCDC710 len 0000294C`) is one of the HEAPREG lines.
//
// Secrets (docs/secrets.md): the base MAC is identity data. This probe prints the full
// address only when it is a placeholder (leading 02:00:00); otherwise it prints
// `MAC|placeholder=0|redacted=1` and nothing else about it.

#include <inttypes.h>
#include <string.h>

#include "esp_chip_info.h"
#include "esp_efuse.h"
#include "esp_efuse_rtc_calib.h"
#include "esp_efuse_table.h"
#include "esp_flash.h"
#include "esp_heap_caps.h"
#include "esp_mac.h"
#include "esp_partition.h"
#include "esp_system.h"
#include "soc/gpio_reg.h"
#include "soc/rtc_cntl_reg.h"
#include "soc/soc.h"

#include "probe_line.h"

#define PROBE_NAME "probe_boot_facts"

// Capability sets worth a total. MALLOC_CAP_INTERNAL|8BIT is the set the earlier prototype
// probes report, so the numbers line up with their captures.
static const struct {
    const char *name;
    uint32_t caps;
} CAP_SETS[] = {
    {"internal_8bit", MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT},
    {"internal_32bit", MALLOC_CAP_INTERNAL | MALLOC_CAP_32BIT},
    {"dma", MALLOC_CAP_DMA},
    {"exec", MALLOC_CAP_EXEC},
    {"rtcram", MALLOC_CAP_RTCRAM},
};

static void print_chip(void)
{
    esp_chip_info_t info;
    esp_chip_info(&info);
    printf("CHIP|model=%d|revision=%u|major=%u|minor=%u|cores=%u|features=0x%08" PRIx32 "\n",
           (int)info.model, (unsigned)info.revision, (unsigned)(info.revision / 100),
           (unsigned)(info.revision % 100), (unsigned)info.cores, (uint32_t)info.features);
}

// Reads one efuse field as an unsigned value, or leaves `*out` untouched and returns false.
static bool efuse_u32(const esp_efuse_desc_t *field[], size_t bits, uint32_t *out)
{
    uint32_t value = 0;
    if (esp_efuse_read_field_blob(field, &value, bits) != ESP_OK) {
        return false;
    }
    *out = value;
    return true;
}

static void print_efuse(void)
{
    uint32_t wafer_major = 0, wafer_lo = 0, wafer_hi = 0, blk_major = 0, blk_minor = 0;
    bool ok = efuse_u32(ESP_EFUSE_WAFER_VERSION_MAJOR, 2, &wafer_major);
    ok &= efuse_u32(ESP_EFUSE_WAFER_VERSION_MINOR_LO, 3, &wafer_lo);
    ok &= efuse_u32(ESP_EFUSE_WAFER_VERSION_MINOR_HI, 1, &wafer_hi);
    ok &= efuse_u32(ESP_EFUSE_BLK_VERSION_MAJOR, 2, &blk_major);
    ok &= efuse_u32(ESP_EFUSE_BLK_VERSION_MINOR, 3, &blk_minor);
    if (!ok) {
        PROBE_FAIL("efuse_read", "esp_efuse_read_field_blob failed");
        return;
    }
    // The C3 wafer minor is split across two fields; the low part is bits 0..2.
    uint32_t wafer_minor = (wafer_hi << 3) | wafer_lo;
    printf("EFUSE|wafer_major=%" PRIu32 "|wafer_minor=%" PRIu32 "|blk_major=%" PRIu32
           "|blk_minor=%" PRIu32 "|pkg_ver=%" PRIu32 "|adc_calib_ver=%d\n",
           wafer_major, wafer_minor, blk_major, blk_minor, esp_efuse_get_pkg_ver(),
           esp_efuse_rtc_calib_get_ver());
}

static void print_reset(void)
{
    // RTC_CNTL_RESET_CAUSE_PROCPU is the raw cause (`RESET_REASON` in IDF `esp32c3/rom/rtc.h`);
    // the IDF enum is the cooked one. Print both so a capture can be read either way.
    uint32_t raw = REG_READ(RTC_CNTL_RESET_STATE_REG) & RTC_CNTL_RESET_CAUSE_PROCPU_M;
    raw >>= RTC_CNTL_RESET_CAUSE_PROCPU_S;
    printf("RESET|reason=%d|raw=0x%02" PRIx32 "\n", (int)esp_reset_reason(), raw);
}

static void print_strap(void)
{
    printf("STRAP|value=0x%02" PRIx32 "\n", REG_READ(GPIO_STRAP_REG) & 0xffu);
}

static void print_flash(void)
{
    uint32_t id = 0, size = 0;
    esp_err_t id_err = esp_flash_read_id(NULL, &id);
    esp_err_t size_err = esp_flash_get_size(NULL, &size);
    if (id_err != ESP_OK || size_err != ESP_OK) {
        PROBE_FAIL("flash_id", "esp_flash_read_id or esp_flash_get_size failed");
        return;
    }
    printf("FLASH|id=0x%06" PRIx32 "|manuf=0x%02" PRIx32 "|type=0x%02" PRIx32
           "|capacity=0x%02" PRIx32 "|size=%" PRIu32 "\n",
           id & 0xffffffu, (id >> 16) & 0xffu, (id >> 8) & 0xffu, id & 0xffu, size);
}

// One line per heap region, emitted by walking every block and folding it into its heap.
// `heap_caps_walk_all` reports the heap bounds with every block, so the first block of a heap is
// enough; later blocks of the same heap are skipped.
#define MAX_HEAPS 16
typedef struct {
    intptr_t start[MAX_HEAPS];
    intptr_t end[MAX_HEAPS];
    size_t count;
} heap_regions_t;

static bool collect_region(walker_heap_into_t heap_info, walker_block_info_t block_info,
                           void *user_data)
{
    (void)block_info;
    heap_regions_t *regions = (heap_regions_t *)user_data;
    for (size_t i = 0; i < regions->count; i++) {
        if (regions->start[i] == heap_info.start) {
            return true;
        }
    }
    if (regions->count < MAX_HEAPS) {
        regions->start[regions->count] = heap_info.start;
        regions->end[regions->count] = heap_info.end;
        regions->count++;
    }
    return true;
}

static void print_heap(void)
{
    static heap_regions_t regions;
    regions.count = 0;
    heap_caps_walk_all(collect_region, &regions);
    for (size_t i = 0; i < regions.count; i++) {
        printf("HEAPREG|index=%u|start=0x%08" PRIxPTR "|end=0x%08" PRIxPTR "|size=%" PRIu32 "\n",
               (unsigned)i, (uintptr_t)regions.start[i], (uintptr_t)regions.end[i],
               (uint32_t)(regions.end[i] - regions.start[i]));
    }
    for (size_t i = 0; i < sizeof(CAP_SETS) / sizeof(CAP_SETS[0]); i++) {
        multi_heap_info_t info;
        memset(&info, 0, sizeof(info));
        heap_caps_get_info(&info, CAP_SETS[i].caps);
        printf("HEAP|%s|total=%u|free=%u|largest=%u|min=%u|blocks=%u\n", CAP_SETS[i].name,
               (unsigned)info.total_free_bytes + (unsigned)info.total_allocated_bytes,
               (unsigned)info.total_free_bytes, (unsigned)info.largest_free_block,
               (unsigned)info.minimum_free_bytes, (unsigned)info.free_blocks);
    }
}

static void print_mac(void)
{
    uint8_t mac[6] = {0};
    if (esp_read_mac(mac, ESP_MAC_BASE) != ESP_OK) {
        PROBE_FAIL("read_mac", "esp_read_mac failed");
        return;
    }
    // Placeholder MACs start 02:00:00. A real address never leaves this probe.
    bool placeholder = mac[0] == 0x02 && mac[1] == 0x00 && mac[2] == 0x00;
    if (placeholder) {
        printf("MAC|placeholder=1|mac=%02x:%02x:%02x:%02x:%02x:%02x\n", mac[0], mac[1], mac[2],
               mac[3], mac[4], mac[5]);
    } else {
        printf("MAC|placeholder=0|redacted=1\n");
    }
}

static void print_partitions(void)
{
    esp_partition_iterator_t it =
        esp_partition_find(ESP_PARTITION_TYPE_ANY, ESP_PARTITION_SUBTYPE_ANY, NULL);
    for (; it != NULL; it = esp_partition_next(it)) {
        const esp_partition_t *part = esp_partition_get(it);
        printf("PART|%s|type=0x%02x|subtype=0x%02x|offset=0x%06" PRIx32 "|size=0x%06" PRIx32
               "|encrypted=%d\n",
               part->label, (unsigned)part->type, (unsigned)part->subtype, part->address,
               part->size, part->encrypted ? 1 : 0);
    }
    esp_partition_iterator_release(it);
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    print_chip();
    print_efuse();
    print_reset();
    print_strap();
    print_flash();
    print_heap();
    print_mac();
    print_partitions();
    PROBE_END(PROBE_NAME, "ok");
}
