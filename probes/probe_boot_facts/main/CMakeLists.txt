# probe_boot_facts main component.
idf_component_register(SRCS "probe_boot_facts.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES efuse esp_hw_support esp_partition esp_system spi_flash heap soc)
