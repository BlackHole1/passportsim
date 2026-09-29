// main/pk_ble.c: Passport Keys BLE 链路:NimBLE GATT 外设。
//
// GATT 定义与 Mac 端 PassportBLE 一致:
//   Service   12D4FA08-7418-48FA-A95A-B43A2E669E55
//   Events    12D4FA09-7418-48FA-A95A-B43A2E669E55   Notify,设备 → Mac
//   Commands  12D4FA0A-7418-48FA-A95A-B43A2E669E55   Write / Write Without Response,Mac → 设备
// 不做配对:链路只传递按键事件与屏幕标签,不涉及敏感数据。
// 同一时间只允许一个连接(CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1),断开后立即恢复广播。
// 广播间隔按 Apple 配件设计指南:开机、断开或 USB 拔线后先以 20 ms 广播 30 秒,之后改为 211.25 ms。
#include "pk_ble.h"

#include "pk_protocol.h"

#include "esp_check.h"
#include "esp_log.h"
#include "host/ble_hs.h"
#include "host/util/util.h"
#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "nvs_flash.h"
#include "services/gap/ble_svc_gap.h"
#include "services/gatt/ble_svc_gatt.h"

#include <string.h>

static const char *TAG = "pk_ble";
static const char *DEVICE_NAME = "Passport Keys";

// 快速广播:Mac app 的扫描占空比较低,200~300 ms 间隔实测要 1~5 秒才被发现,20 ms 间隔可降到几百毫秒。
#define ADV_FAST_ITVL  BLE_GAP_ADV_ITVL_MS(20)
#define ADV_FAST_MS    30000
// 之后改用指南推荐的长间隔 211.25 ms(单位 0.625 ms),降低功耗。
#define ADV_SLOW_ITVL  338

// NimBLE 的 128 位 UUID 按小端字节序书写,即 UUID 字符串字节倒序;三个 UUID 只有第一段末字节不同。
#define PK_UUID128(first_group_last_byte)                                  \
    BLE_UUID128_INIT(0x55, 0x9E, 0x66, 0x2E, 0x3A, 0xB4, 0x5A, 0xA9,       \
                     0xFA, 0x48, 0x18, 0x74, (first_group_last_byte), 0xFA, 0xD4, 0x12)

static const ble_uuid128_t s_service_uuid = PK_UUID128(0x08);
static const ble_uuid128_t s_events_uuid = PK_UUID128(0x09);
static const ble_uuid128_t s_commands_uuid = PK_UUID128(0x0A);

static pk_ble_line_cb_t s_on_line;
static pk_ble_link_cb_t s_on_link;
static uint16_t s_events_handle;
static uint8_t s_own_addr_type;

// 由 host 任务写入,pk_ble_send() 在应用任务中读取。读到过期值最多让一次 Notify 失败,
// ble_gatts_notify_custom() 会拒绝已失效的连接句柄。
static volatile uint16_t s_conn_handle = BLE_HS_CONN_HANDLE_NONE;
static volatile bool s_subscribed;

// 只在 host 任务中访问。
static pk_line_t s_rx_line;

// pk_ble_boost_advertising() 通过它把广播切换投递到 host 任务,与 gap_event() 串行执行。
static struct ble_npl_event s_boost_event;
static bool s_started;

static int gap_event(struct ble_gap_event *event, void *arg);

static void dispatch_line(const char *line, size_t len, void *user)
{
    (void)user;
    s_on_line(line, len);
}

static int events_access(uint16_t conn_handle, uint16_t attr_handle,
                         struct ble_gatt_access_ctxt *ctxt, void *arg)
{
    (void)conn_handle;
    (void)attr_handle;
    (void)ctxt;
    (void)arg;
    // 事件特征只支持 Notify,没有可读的值。
    return BLE_ATT_ERR_READ_NOT_PERMITTED;
}

static int commands_access(uint16_t conn_handle, uint16_t attr_handle,
                           struct ble_gatt_access_ctxt *ctxt, void *arg)
{
    (void)conn_handle;
    (void)attr_handle;
    (void)arg;
    if (ctxt->op != BLE_GATT_ACCESS_OP_WRITE_CHR) return BLE_ATT_ERR_UNLIKELY;

    uint8_t buf[PK_LINE_MAX];
    uint16_t len = 0;
    if (OS_MBUF_PKTLEN(ctxt->om) > sizeof(buf)) return BLE_ATT_ERR_INVALID_ATTR_VALUE_LEN;
    if (ble_hs_mbuf_to_flat(ctxt->om, buf, sizeof(buf), &len) != 0) return BLE_ATT_ERR_UNLIKELY;

    // 一次写入可能只包含半行,也可能包含多行,统一交给行组帧。
    pk_line_feed(&s_rx_line, buf, len, dispatch_line, NULL);
    return 0;
}

static const struct ble_gatt_svc_def s_services[] = {
    {
        .type = BLE_GATT_SVC_TYPE_PRIMARY,
        .uuid = &s_service_uuid.u,
        .characteristics = (struct ble_gatt_chr_def[]) {
            {
                .uuid = &s_events_uuid.u,
                .access_cb = events_access,
                .val_handle = &s_events_handle,
                .flags = BLE_GATT_CHR_F_NOTIFY,
            },
            {
                .uuid = &s_commands_uuid.u,
                .access_cb = commands_access,
                .flags = BLE_GATT_CHR_F_WRITE | BLE_GATT_CHR_F_WRITE_NO_RSP,
            },
            { 0 },
        },
    },
    { 0 },
};

static void start_advertising(bool fast)
{
    // 主广播包放服务 UUID,Mac 按服务过滤扫描;名称放扫描响应,两者合计会超过 31 字节。
    struct ble_hs_adv_fields fields = { 0 };
    fields.flags = BLE_HS_ADV_F_DISC_GEN | BLE_HS_ADV_F_BREDR_UNSUP;
    fields.uuids128 = &s_service_uuid;
    fields.num_uuids128 = 1;
    fields.uuids128_is_complete = 1;
    int rc = ble_gap_adv_set_fields(&fields);
    if (rc != 0) {
        ESP_LOGE(TAG, "set adv fields failed: %d", rc);
        return;
    }

    struct ble_hs_adv_fields rsp = { 0 };
    rsp.name = (const uint8_t *)DEVICE_NAME;
    rsp.name_len = (uint8_t)strlen(DEVICE_NAME);
    rsp.name_is_complete = 1;
    rc = ble_gap_adv_rsp_set_fields(&rsp);
    if (rc != 0) {
        ESP_LOGE(TAG, "set scan response failed: %d", rc);
        return;
    }

    struct ble_gap_adv_params params = { 0 };
    params.conn_mode = BLE_GAP_CONN_MODE_UND;
    params.disc_mode = BLE_GAP_DISC_MODE_GEN;
    params.itvl_min = fast ? ADV_FAST_ITVL : ADV_SLOW_ITVL;
    params.itvl_max = params.itvl_min;
    // 快速广播到期后触发 BLE_GAP_EVENT_ADV_COMPLETE,在那里切到慢速广播。
    rc = ble_gap_adv_start(s_own_addr_type, NULL, fast ? ADV_FAST_MS : BLE_HS_FOREVER, &params, gap_event, NULL);
    if (rc != 0 && rc != BLE_HS_EALREADY) {
        ESP_LOGE(TAG, "adv start failed: %d", rc);
    }
}

static void request_connection_params(uint16_t conn_handle)
{
    // 按 Apple 配件设计指南取值:间隔 15~30 ms,从机延迟 4,监督超时 4 s。
    // 从机延迟只推迟 Mac → 设备方向;设备有按键要发时会在下一个连接事件立即发送。
    const struct ble_gap_upd_params params = {
        .itvl_min = BLE_GAP_CONN_ITVL_MS(15),
        .itvl_max = BLE_GAP_CONN_ITVL_MS(30),
        .latency = 4,
        .supervision_timeout = BLE_GAP_SUPERVISION_TIMEOUT_MS(4000),
        .min_ce_len = 0,
        .max_ce_len = 0,
    };
    int rc = ble_gap_update_params(conn_handle, &params);
    if (rc != 0) {
        ESP_LOGW(TAG, "connection params request failed: %d", rc);
    }
}

static void set_subscribed(bool subscribed)
{
    if (subscribed == s_subscribed) return;
    s_subscribed = subscribed;
    s_on_link(subscribed);
}

static int gap_event(struct ble_gap_event *event, void *arg)
{
    (void)arg;
    switch (event->type) {
    case BLE_GAP_EVENT_CONNECT:
        if (event->connect.status == 0) {
            s_conn_handle = event->connect.conn_handle;
            pk_line_init(&s_rx_line);
            ESP_LOGI(TAG, "connected");
        } else {
            start_advertising(true);
        }
        break;
    case BLE_GAP_EVENT_DISCONNECT:
        ESP_LOGI(TAG, "disconnected, reason=0x%x", event->disconnect.reason);
        s_conn_handle = BLE_HS_CONN_HANDLE_NONE;
        set_subscribed(false);
        start_advertising(true);
        break;
    case BLE_GAP_EVENT_SUBSCRIBE:
        if (event->subscribe.attr_handle == s_events_handle) {
            set_subscribed(event->subscribe.cur_notify);
            // Mac 订阅时服务发现已经结束,此时再请求连接参数,不干扰发现过程。
            if (event->subscribe.cur_notify) request_connection_params(event->subscribe.conn_handle);
        }
        break;
    case BLE_GAP_EVENT_ADV_COMPLETE:
        if (s_conn_handle == BLE_HS_CONN_HANDLE_NONE) start_advertising(false);
        break;
    case BLE_GAP_EVENT_MTU:
        ESP_LOGI(TAG, "mtu=%d", event->mtu.value);
        break;
    default:
        break;
    }
    return 0;
}

static void on_sync(void)
{
    int rc = ble_hs_util_ensure_addr(0);
    if (rc == 0) rc = ble_hs_id_infer_auto(0, &s_own_addr_type);
    if (rc != 0) {
        ESP_LOGE(TAG, "address setup failed: %d", rc);
        return;
    }
    start_advertising(true);
}

static void on_reset(int reason)
{
    // 控制器复位后 NimBLE 会重新同步并再次调用 on_sync(),在那里恢复广播。
    ESP_LOGW(TAG, "host reset, reason=%d", reason);
    s_conn_handle = BLE_HS_CONN_HANDLE_NONE;
    set_subscribed(false);
}

static void on_boost_event(struct ble_npl_event *ev)
{
    (void)ev;
    if (!ble_hs_synced() || s_conn_handle != BLE_HS_CONN_HANDLE_NONE) return;
    // 正在慢速广播时先停止再以快速间隔重新开始;已在快速广播时重新计时 30 秒。停止广播不会触发 ADV_COMPLETE。
    ble_gap_adv_stop();
    start_advertising(true);
}

static void host_task(void *arg)
{
    (void)arg;
    // 本应用不停止 BLE,正常情况下 nimble_port_run() 不会返回。
    nimble_port_run();
    nimble_port_freertos_deinit();
}

esp_err_t pk_ble_start(pk_ble_line_cb_t on_line, pk_ble_link_cb_t on_link)
{
    s_on_line = on_line;
    s_on_link = on_link;
    pk_line_init(&s_rx_line);

    // 控制器需要 NVS 保存 PHY 校准数据。与基线一致:初始化失败时不擦除分区,避免清掉已有数据。
    ESP_RETURN_ON_ERROR(nvs_flash_init(), TAG, "nvs init failed (partition not erased)");
    ESP_RETURN_ON_ERROR(nimble_port_init(), TAG, "nimble init failed");

    ble_svc_gap_init();
    ble_svc_gatt_init();
    int rc = ble_gatts_count_cfg(s_services);
    if (rc == 0) rc = ble_gatts_add_svcs(s_services);
    if (rc == 0) rc = ble_svc_gap_device_name_set(DEVICE_NAME);
    if (rc != 0) {
        ESP_LOGE(TAG, "gatt setup failed: %d", rc);
        nimble_port_deinit();
        return ESP_FAIL;
    }

    ble_npl_event_init(&s_boost_event, on_boost_event, NULL);
    ble_hs_cfg.sync_cb = on_sync;
    ble_hs_cfg.reset_cb = on_reset;
    nimble_port_freertos_init(host_task);
    s_started = true;
    return ESP_OK;
}

void pk_ble_boost_advertising(void)
{
    if (!s_started) return;
    // 同一事件已在队列中时不会重复入队。
    ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_boost_event);
}

void pk_ble_send(const char *data, size_t len)
{
    uint16_t conn = s_conn_handle;
    if (len == 0 || !s_subscribed || conn == BLE_HS_CONN_HANDLE_NONE) return;

    // MTU 协商前每次 Notify 只能带 20 字节;按当前 MTU 分片,Mac 端按 '\n' 重组。
    uint16_t mtu = ble_att_mtu(conn);
    size_t chunk = mtu > 3 ? (size_t)mtu - 3 : 20;
    for (size_t offset = 0; offset < len; offset += chunk) {
        size_t n = len - offset < chunk ? len - offset : chunk;
        struct os_mbuf *om = ble_hs_mbuf_from_flat(data + offset, (uint16_t)n);
        if (!om) {
            ESP_LOGW(TAG, "mbuf alloc failed");
            return;
        }
        // 无论成功与否 om 都由协议栈释放。
        int rc = ble_gatts_notify_custom(conn, s_events_handle, om);
        if (rc != 0) {
            ESP_LOGW(TAG, "notify failed: %d", rc);
            return;
        }
    }
}
