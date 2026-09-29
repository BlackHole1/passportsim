# probe_wifi_http main component.
idf_component_register(SRCS "probe_wifi_http.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_event esp_http_client esp_netif esp_wifi freertos lwip
                                     mbedtls nvs_flash)
