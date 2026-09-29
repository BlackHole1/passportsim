# probe_wdt main component.
idf_component_register(SRCS "probe_wdt.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_system freertos soc)
