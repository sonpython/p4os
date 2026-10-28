# Sensitivity check, layered on sdkconfig.defaults: the same app with the
# debug-oriented IDF defaults switched off, to show how much of the
# FreeRTOS overhead they account for.
CONFIG_COMPILER_OPTIMIZATION_ASSERTIONS_DISABLE=y
# CONFIG_ESP_SYSTEM_HW_STACK_GUARD is not set
CONFIG_FREERTOS_CHECK_STACKOVERFLOW_NONE=y
