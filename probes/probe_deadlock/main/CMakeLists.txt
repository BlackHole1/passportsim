# probe_deadlock main component.
idf_component_register(SRCS "probe_deadlock.c"
                       INCLUDE_DIRS "." "../../common"
                       PRIV_REQUIRES freertos)
