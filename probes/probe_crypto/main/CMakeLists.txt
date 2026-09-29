# probe_crypto main component.
idf_component_register(SRCS "probe_crypto.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_timer mbedtls)
