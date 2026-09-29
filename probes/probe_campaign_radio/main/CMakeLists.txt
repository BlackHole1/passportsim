# probe_campaign_radio main component (specs/notes/silicon-campaign.md).
idf_component_register(SRCS "probe_campaign_radio.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES bt esp_timer freertos heap log)
