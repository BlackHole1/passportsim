# probe_wifi_conn main component.
idf_component_register(SRCS "probe_wifi_conn.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_event esp_netif esp_wifi freertos nvs_flash)
