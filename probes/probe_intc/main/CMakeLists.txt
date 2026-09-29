# probe_intc main component.
idf_component_register(SRCS "probe_intc.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_hw_support esp_system freertos soc)
