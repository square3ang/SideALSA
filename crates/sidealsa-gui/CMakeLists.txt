cmake_minimum_required(VERSION 3.20)

project(sidealsa-control LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Qt6 6.5 REQUIRED COMPONENTS Widgets)

qt_add_executable(sidealsa-control src/main.cpp)
qt_add_resources(sidealsa-control "sidealsa_icons"
    PREFIX "/icons"
    BASE "${CMAKE_CURRENT_SOURCE_DIR}/../../assets"
    FILES "${CMAKE_CURRENT_SOURCE_DIR}/../../assets/sidealsa-icon.png"
)
target_link_libraries(sidealsa-control PRIVATE Qt6::Widgets)
target_compile_options(sidealsa-control PRIVATE -Wall -Wextra -Wpedantic)

install(TARGETS sidealsa-control RUNTIME DESTINATION bin)
install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/../../assets/sidealsa-icon.png"
    DESTINATION share/icons/hicolor/512x512/apps
    RENAME org.sidealsa.Control.png
)
