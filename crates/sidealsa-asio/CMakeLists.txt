cmake_minimum_required(VERSION 3.20)

project(sidealsa-asio C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_EXTENSIONS ON)

find_program(CARGO_EXECUTABLE cargo REQUIRED)
find_program(WINEGCC_EXECUTABLE winegcc REQUIRED)
find_program(WINEBUILD_EXECUTABLE winebuild REQUIRED)
find_path(WINE_INCLUDE_DIR wine/debug.h PATHS /usr/include/wine REQUIRED)
find_path(WINE_WINDOWS_INCLUDE_DIR windows.h PATHS /usr/include/wine/windows REQUIRED)

set(SIDEALSA_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../..")
set(CARGO_TARGET_DIR "${CMAKE_CURRENT_BINARY_DIR}/cargo-target")
set(RUST_PROFILE debug)
set(CARGO_PROFILE_ARGS)
if(CMAKE_BUILD_TYPE MATCHES "^(Release|RelWithDebInfo|MinSizeRel)$")
    set(RUST_PROFILE release)
    set(CARGO_PROFILE_ARGS --release)
endif()

set(RUST_LIBRARY "${CARGO_TARGET_DIR}/${RUST_PROFILE}/libsidealsa_asio.a")
set(RUST_SOURCES
    "${CMAKE_CURRENT_SOURCE_DIR}/src/lib.rs"
    "${SIDEALSA_ROOT}/crates/sidealsa-client/src/lib.rs"
    "${SIDEALSA_ROOT}/crates/sidealsa-client/src/shared.rs"
    "${SIDEALSA_ROOT}/crates/sidealsa-protocol/src/lib.rs")
set(DLL_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/sidealsa-asio64.dll")
set(UNIX_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/sidealsa-asio64.dll.so")
set(PROBE_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/sidealsa-asio-probe.exe")
set(LOOPBACK_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/sidealsa-asio-loopback-test.exe")
set(LIFECYCLE_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/sidealsa-asio-lifecycle-probe.exe")
set(WINE_WINDOWS_OUTPUT
    "${CMAKE_CURRENT_BINARY_DIR}/x86_64-windows/sidealsa-asio64.dll")
set(WINE_UNIX_OUTPUT
    "${CMAKE_CURRENT_BINARY_DIR}/x86_64-unix/sidealsa-asio64.dll.so")
set(WINE_WINDOWS_ALIAS
    "${CMAKE_CURRENT_BINARY_DIR}/x86_64-windows/sidealsa-asio.dll")
set(WINE_UNIX_ALIAS
    "${CMAKE_CURRENT_BINARY_DIR}/x86_64-unix/sidealsa-asio.dll.so")

add_library(sidealsa-asio-shim OBJECT src/wine_shim.c)
set_target_properties(sidealsa-asio-shim PROPERTIES POSITION_INDEPENDENT_CODE ON)
target_include_directories(sidealsa-asio-shim PRIVATE
    "${CMAKE_CURRENT_SOURCE_DIR}/include"
    "${WINE_INCLUDE_DIR}"
    "${WINE_WINDOWS_INCLUDE_DIR}")
target_compile_options(sidealsa-asio-shim PRIVATE
    -D_REENTRANT
    -fno-lto
    -Wall
    -Werror=implicit-function-declaration)

add_custom_command(
    OUTPUT "${RUST_LIBRARY}"
    COMMAND "${CARGO_EXECUTABLE}" build --package sidealsa-asio
            --target-dir "${CARGO_TARGET_DIR}" ${CARGO_PROFILE_ARGS}
    WORKING_DIRECTORY "${SIDEALSA_ROOT}"
    DEPENDS
        "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
        ${RUST_SOURCES}
        "${SIDEALSA_ROOT}/Cargo.toml"
        "${SIDEALSA_ROOT}/Cargo.lock"
    VERBATIM
)

add_custom_command(
    OUTPUT "${DLL_OUTPUT}"
    COMMAND "${WINEBUILD_EXECUTABLE}" -m64 --dll --fake-module
            -E "${CMAKE_CURRENT_SOURCE_DIR}/sidealsa-asio64.dll.spec"
            $<TARGET_OBJECTS:sidealsa-asio-shim>
            -o "${DLL_OUTPUT}"
    DEPENDS
        sidealsa-asio-shim
        "${CMAKE_CURRENT_SOURCE_DIR}/sidealsa-asio64.dll.spec"
    VERBATIM
)

add_custom_command(
    OUTPUT "${UNIX_OUTPUT}"
    COMMAND "${WINEGCC_EXECUTABLE}" -m64 -shared
            "${CMAKE_CURRENT_SOURCE_DIR}/sidealsa-asio64.dll.spec"
            $<TARGET_OBJECTS:sidealsa-asio-shim>
            "${RUST_LIBRARY}"
            -lpthread -ldl -lm -lole32 -luuid
            -o "${UNIX_OUTPUT}"
    DEPENDS
        "${RUST_LIBRARY}"
        sidealsa-asio-shim
        "${CMAKE_CURRENT_SOURCE_DIR}/sidealsa-asio64.dll.spec"
    VERBATIM
)

add_custom_command(
    OUTPUT "${DLL_OUTPUT}.stamp"
    COMMAND "${CMAKE_COMMAND}" -E touch "${DLL_OUTPUT}.stamp"
    DEPENDS "${DLL_OUTPUT}" "${UNIX_OUTPUT}"
)

add_custom_target(sidealsa-asio ALL DEPENDS "${DLL_OUTPUT}.stamp")

add_custom_command(
    OUTPUT "${WINE_WINDOWS_OUTPUT}" "${WINE_UNIX_OUTPUT}"
           "${WINE_WINDOWS_ALIAS}" "${WINE_UNIX_ALIAS}"
    COMMAND "${CMAKE_COMMAND}" -E make_directory
            "${CMAKE_CURRENT_BINARY_DIR}/x86_64-windows"
            "${CMAKE_CURRENT_BINARY_DIR}/x86_64-unix"
    COMMAND "${CMAKE_COMMAND}" -E create_symlink
            "../sidealsa-asio64.dll" "${WINE_WINDOWS_OUTPUT}"
    COMMAND "${CMAKE_COMMAND}" -E create_symlink
            "../sidealsa-asio64.dll.so" "${WINE_UNIX_OUTPUT}"
    COMMAND "${CMAKE_COMMAND}" -E create_symlink
            "sidealsa-asio64.dll" "${WINE_WINDOWS_ALIAS}"
    COMMAND "${CMAKE_COMMAND}" -E create_symlink
            "sidealsa-asio64.dll.so" "${WINE_UNIX_ALIAS}"
    DEPENDS "${DLL_OUTPUT}" "${UNIX_OUTPUT}"
    VERBATIM
)

add_custom_target(sidealsa-asio-runtime-layout
    DEPENDS "${WINE_WINDOWS_OUTPUT}" "${WINE_UNIX_OUTPUT}"
            "${WINE_WINDOWS_ALIAS}" "${WINE_UNIX_ALIAS}")
add_dependencies(sidealsa-asio sidealsa-asio-runtime-layout)

add_custom_command(
    OUTPUT "${PROBE_OUTPUT}" "${PROBE_OUTPUT}.so"
    COMMAND "${WINEGCC_EXECUTABLE}" -m64 -std=gnu11 -O2 -Wall -Wextra
            "${CMAKE_CURRENT_SOURCE_DIR}/tests/asio_probe.c"
            -lole32 -luuid -lm
            -o "${PROBE_OUTPUT}"
    DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/tests/asio_probe.c"
    VERBATIM
)

add_custom_target(sidealsa-asio-probe DEPENDS "${PROBE_OUTPUT}")
add_dependencies(sidealsa-asio-probe sidealsa-asio-runtime-layout)

add_custom_command(
    OUTPUT "${LOOPBACK_OUTPUT}" "${LOOPBACK_OUTPUT}.so"
    COMMAND "${WINEGCC_EXECUTABLE}" -m64 -std=gnu11 -O2 -Wall -Wextra
            -DSIDEALSA_ASIO_LOOPBACK_DEFAULT=1
            "${CMAKE_CURRENT_SOURCE_DIR}/tests/asio_probe.c"
            -lole32 -luuid -lm
            -o "${LOOPBACK_OUTPUT}"
    DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/tests/asio_probe.c"
    VERBATIM
)

add_custom_target(sidealsa-asio-loopback-test DEPENDS "${LOOPBACK_OUTPUT}")
add_dependencies(sidealsa-asio-loopback-test sidealsa-asio-runtime-layout)

add_custom_command(
    OUTPUT "${LIFECYCLE_OUTPUT}" "${LIFECYCLE_OUTPUT}.so"
    COMMAND "${WINEGCC_EXECUTABLE}" -m64 -std=gnu11 -O2 -Wall -Wextra
            -DSIDEALSA_ASIO_LIFECYCLE_DEFAULT=1
            "${CMAKE_CURRENT_SOURCE_DIR}/tests/asio_probe.c"
            -lole32 -luuid -lm
            -o "${LIFECYCLE_OUTPUT}"
    DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/tests/asio_probe.c"
    VERBATIM
)

add_custom_target(sidealsa-asio-lifecycle-probe DEPENDS "${LIFECYCLE_OUTPUT}")
add_dependencies(sidealsa-asio-lifecycle-probe sidealsa-asio-runtime-layout)

include(CTest)
if(BUILD_TESTING)
    find_program(WINE_EXECUTABLE NAMES wine wine64)
    if(WINE_EXECUTABLE)
        add_test(NAME sidealsa-asio-sine-self-test
            COMMAND "${CMAKE_COMMAND}"
                "-DBUILD_DIR=${CMAKE_CURRENT_BINARY_DIR}"
                "-DPROBE=${PROBE_OUTPUT}.so"
                "-DWINE=${WINE_EXECUTABLE}"
                -P "${CMAKE_CURRENT_SOURCE_DIR}/tests/sine_self_test.cmake")
        set_tests_properties(sidealsa-asio-sine-self-test PROPERTIES
            TIMEOUT 120)
    endif()
endif()

install(FILES "${DLL_OUTPUT}" DESTINATION lib/wine/x86_64-windows)
install(FILES "${UNIX_OUTPUT}" DESTINATION lib/wine/x86_64-unix)
install(FILES "${WINE_WINDOWS_ALIAS}" DESTINATION lib/wine/x86_64-windows)
install(FILES "${WINE_UNIX_ALIAS}" DESTINATION lib/wine/x86_64-unix)
