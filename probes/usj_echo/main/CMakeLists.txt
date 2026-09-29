# usj_echo main component.
idf_component_register(SRCS "usj_echo.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES esp_driver_usb_serial_jtag esp_timer freertos)
