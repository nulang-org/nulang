cmake_minimum_required(VERSION 3.20)
project(nulang_caf_cross_runtime LANGUAGES CXX)

find_package(CAF COMPONENTS core REQUIRED)

add_executable(caf_baseline main.cpp)
target_compile_features(caf_baseline PRIVATE cxx_std_20)
target_link_libraries(caf_baseline PRIVATE CAF::core)
