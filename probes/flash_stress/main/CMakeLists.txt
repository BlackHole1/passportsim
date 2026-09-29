# flash_stress main component.
idf_component_register(SRCS "flash_stress.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_partition esp_rom esp_system esp_timer freertos spi_flash)
