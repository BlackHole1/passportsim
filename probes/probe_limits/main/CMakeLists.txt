# probe_limits main component.
idf_component_register(SRCS "probe_limits.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_app_format heap)
