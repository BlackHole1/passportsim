# sleep_timer main component.
idf_component_register(SRCS "sleep_timer.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_hw_support esp_system esp_timer freertos soc)
