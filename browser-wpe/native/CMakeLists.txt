cmake_minimum_required(VERSION 3.22)
project(waterui_wpe C)

find_package(PkgConfig REQUIRED)
pkg_check_modules(WPE REQUIRED IMPORTED_TARGET
    wpe-webkit-2.0
    wpe-platform-2.0
    libsoup-3.0
    json-glib-1.0
    libdrm)
pkg_check_modules(WPE_EXTENSION REQUIRED IMPORTED_TARGET
    wpe-web-process-extension-2.0)

add_library(waterui_wpe SHARED waterui_wpe.c)
target_compile_features(waterui_wpe PRIVATE c_std_11)
# dladdr/Dl_info in <dlfcn.h> are exposed only under _GNU_SOURCE.
target_compile_definitions(waterui_wpe PRIVATE _GNU_SOURCE)
target_compile_options(waterui_wpe PRIVATE
    -Wall
    -Wextra
    -Werror
    -Wpedantic)
target_link_libraries(waterui_wpe PRIVATE PkgConfig::WPE dl)
set_target_properties(waterui_wpe PROPERTIES
    BUILD_RPATH "$ORIGIN"
    INSTALL_RPATH "$ORIGIN")
install(TARGETS waterui_wpe LIBRARY DESTINATION lib)
install(FILES waterui_wpe.h DESTINATION include)

add_library(waterui_wpe_extension MODULE waterui_wpe_extension.c)
target_compile_features(waterui_wpe_extension PRIVATE c_std_11)
target_compile_options(waterui_wpe_extension PRIVATE
    -Wall
    -Wextra
    -Werror
    -Wpedantic)
target_link_libraries(waterui_wpe_extension PRIVATE
    PkgConfig::WPE_EXTENSION)
set_target_properties(waterui_wpe_extension PROPERTIES
    PREFIX ""
    BUILD_RPATH "$ORIGIN/../.."
    INSTALL_RPATH "$ORIGIN/../..")
install(TARGETS waterui_wpe_extension
    LIBRARY DESTINATION lib/waterui-wpe/extensions)
if(DEFINED WATER_WPE_EXPECTED_ABI_VERSION)
    target_compile_definitions(waterui_wpe PRIVATE
        WATER_WPE_EXPECTED_ABI_VERSION=${WATER_WPE_EXPECTED_ABI_VERSION})
    target_compile_definitions(waterui_wpe_extension PRIVATE
        WATER_WPE_EXPECTED_ABI_VERSION=${WATER_WPE_EXPECTED_ABI_VERSION})
endif()
option(WATER_WPE_BUILD_LIFETIME_TEST
    "Build the page lifetime whitebox fixture as a test shared object"
    OFF)
if(WATER_WPE_BUILD_LIFETIME_TEST)
    if(NOT DEFINED WATER_WPE_TEST_RUNTIME_ROOT)
        message(FATAL_ERROR
            "WATER_WPE_TEST_RUNTIME_ROOT must name the staged runtime root the fixture lands in")
    endif()
    if(NOT DEFINED WATER_WPE_TEST_OUTPUT_NAME)
        message(FATAL_ERROR
            "WATER_WPE_TEST_OUTPUT_NAME must name the fixture's output library")
    endif()
    # A separate shared object that includes the bridge source — the only way
    # a test can inspect the private binding and page state. It installs
    # nowhere and never ships in an application.
    add_library(wpe_lifetime_test SHARED page_lifetime_test.c)
    target_compile_features(wpe_lifetime_test PRIVATE c_std_11)
    target_compile_definitions(wpe_lifetime_test PRIVATE _GNU_SOURCE)
    target_compile_options(wpe_lifetime_test PRIVATE
        -Wall
        -Wextra
        -Werror
        -Wpedantic)
    target_link_libraries(wpe_lifetime_test PRIVATE PkgConfig::WPE dl)
    set_target_properties(wpe_lifetime_test PROPERTIES
        OUTPUT_NAME "${WATER_WPE_TEST_OUTPUT_NAME}"
        LIBRARY_OUTPUT_DIRECTORY "${WATER_WPE_TEST_RUNTIME_ROOT}/lib"
        BUILD_RPATH "$ORIGIN")
endif()
