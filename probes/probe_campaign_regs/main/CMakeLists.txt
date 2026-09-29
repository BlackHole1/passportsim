# probe_campaign_regs main component (specs/notes/silicon-campaign.md).
idf_component_register(SRCS "probe_campaign_regs.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_adc esp_driver_usb_serial_jtag esp_hw_support esp_rom esp_timer spi_flash)
