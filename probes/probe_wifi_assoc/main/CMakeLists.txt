# probe_wifi_assoc main component.
idf_component_register(SRCS "probe_wifi_assoc.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_event esp_netif esp_timer esp_wifi freertos nvs_flash)
