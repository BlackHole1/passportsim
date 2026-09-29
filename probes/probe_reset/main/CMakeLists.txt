# probe_reset main component.
idf_component_register(SRCS "probe_reset.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_hw_support esp_system freertos soc)
