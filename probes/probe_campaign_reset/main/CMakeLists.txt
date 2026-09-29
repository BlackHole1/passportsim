# probe_campaign_reset main component (specs/notes/silicon-campaign.md).
idf_component_register(SRCS "probe_campaign_reset.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_driver_usb_serial_jtag esp_hw_support esp_system esp_timer freertos soc)
