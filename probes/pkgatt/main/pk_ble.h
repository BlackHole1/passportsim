// main/pk_ble.h: BLE 链路:NimBLE GATT 外设,广播 Passport Keys 服务。
#pragma once

#include "esp_err.h"

#include <stdbool.h>
#include <stddef.h>

// 每收到完整一行命令调用一次,运行在 NimBLE host 任务中,必须快速返回。
typedef void (*pk_ble_line_cb_t)(const char *line, size_t len);
// Mac 订阅或取消订阅事件特征(含断开连接)时调用,运行在 NimBLE host 任务中。
typedef void (*pk_ble_link_cb_t)(bool subscribed);

// 初始化 NVS 与 NimBLE,注册 GATT 服务并开始可连接广播。
esp_err_t pk_ble_start(pk_ble_line_cb_t on_line, pk_ble_link_cb_t on_link);

// 通过事件特征 Notify 一帧,按 MTU 分片;未连接或未订阅时丢弃。可从任意任务调用。
void pk_ble_send(const char *data, size_t len);

// 切换到 30 秒快速广播,让 Mac 尽快发现设备;已连接或 BLE 未启动时忽略。可从任意任务调用。
void pk_ble_boost_advertising(void);
