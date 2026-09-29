# hle_probe main component.
idf_component_register(SRCS "hle_probe.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_event esp_hw_support freertos heap soc)
