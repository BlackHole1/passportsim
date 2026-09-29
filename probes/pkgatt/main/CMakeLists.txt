# pkgatt main component.
# The component list is the earlier prototype's.
# pk_ble.c, pk_ble.h, pk_protocol.c and pk_protocol.h are Passport Keys sources, MIT, copyright
# FoloToy, unmodified; see LICENSE.passport-keys beside them.
idf_component_register(SRCS "pkgatt.c" "pk_ble.c" "pk_protocol.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES bt nvs_flash freertos heap esp_timer)
