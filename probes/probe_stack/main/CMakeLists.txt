# probe_stack main component.
idf_component_register(SRCS "probe_stack.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_system freertos soc)
