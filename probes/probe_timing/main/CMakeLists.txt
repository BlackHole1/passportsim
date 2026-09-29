# probe_timing main component.
idf_component_register(SRCS "probe_timing.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_driver_gpio esp_driver_i2c esp_driver_spi esp_partition esp_rom esp_timer
                                     mbedtls spi_flash)
