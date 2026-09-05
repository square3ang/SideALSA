# Rebuild before execution: an old probe could ignore the self-test environment
# variable and enter its normal hardware test instead.
execute_process(
    COMMAND "${CMAKE_COMMAND}" --build "${BUILD_DIR}" --target sidealsa-asio-probe
    RESULT_VARIABLE build_status)
if(NOT build_status STREQUAL "0")
    message(FATAL_ERROR "Could not build the sine self-test probe")
endif()

file(STRINGS "${PROBE}" self_test_marker REGEX "sine self-test PASS:" LIMIT_COUNT 1)
if(NOT self_test_marker)
    message(FATAL_ERROR "Refusing to execute a probe without the sine self-test marker")
endif()

execute_process(
    COMMAND "${CMAKE_COMMAND}" -E env SIDEALSA_ASIO_PROBE_SINE_SELF_TEST=1
        "${WINE}" "${PROBE}"
    RESULT_VARIABLE status
    OUTPUT_VARIABLE output
    ERROR_VARIABLE errors
    TIMEOUT 90)
if(NOT status STREQUAL "0" OR NOT "${output}${errors}" MATCHES "sine self-test PASS:")
    message(FATAL_ERROR "Sine self-test failed (${status}):\n${output}${errors}")
endif()
message(STATUS "${output}${errors}")
