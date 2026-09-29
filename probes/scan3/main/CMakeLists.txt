# scan3 main component.
# The component list is the earlier prototype's.
idf_component_register(SRCS "scan3.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_wifi esp_netif esp_event nvs_flash bt lwip freertos heap esp_timer)
