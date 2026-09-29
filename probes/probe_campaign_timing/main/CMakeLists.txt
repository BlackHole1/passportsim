# probe_campaign_timing main component (specs/notes/silicon-campaign.md).
idf_component_register(SRCS "probe_campaign_timing.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_driver_i2c esp_driver_uart esp_hw_support esp_security
                                     esp_timer hal mbedtls)
