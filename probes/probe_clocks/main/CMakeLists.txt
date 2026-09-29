# probe_clocks main component.
idf_component_register(SRCS "probe_clocks.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_hw_support esp_timer esp_system freertos soc)
