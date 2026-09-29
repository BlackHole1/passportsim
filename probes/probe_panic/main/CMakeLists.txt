# probe_panic main component.
idf_component_register(SRCS "probe_panic.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_system freertos soc)
