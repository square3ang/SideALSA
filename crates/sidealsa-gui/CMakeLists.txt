cmake_minimum_required(VERSION 3.20)

project(sidealsa-control LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Qt6 6.5 REQUIRED COMPONENTS Widgets)

qt_add_executable(sidealsa-control src/main.cpp)
target_link_libraries(sidealsa-control PRIVATE Qt6::Widgets)
target_compile_options(sidealsa-control PRIVATE -Wall -Wextra -Wpedantic)

install(TARGETS sidealsa-control RUNTIME DESTINATION bin)
