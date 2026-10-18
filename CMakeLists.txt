cmake_minimum_required(VERSION 3.25)
project(fooyin_tak VERSION 0.1.1 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
include(CTest)
include(GNUInstallDirs)

find_package(Fooyin 0.13.1 CONFIG REQUIRED)
find_package(Threads REQUIRED)
find_program(CARGO_EXECUTABLE cargo REQUIRED)

set(TAK_PLUGIN_INSTALL_DIR "${FOOYIN_PLUGIN_INSTALL_DIR}" CACHE STRING
    "Plugin directory (absolute, or relative to the installation prefix)")
set(FOOYIN_PLUGIN_INSTALL_DIR "${TAK_PLUGIN_INSTALL_DIR}")
set(TAK_CARGO_TARGET_DIR "${CMAKE_CURRENT_BINARY_DIR}/cargo")
if(MSVC)
    set(TAK_RUST_LIBRARY "${TAK_CARGO_TARGET_DIR}/release/fooyin_tak.lib")
else()
    set(TAK_RUST_LIBRARY "${TAK_CARGO_TARGET_DIR}/release/libfooyin_tak.a")
endif()

add_custom_command(
    OUTPUT "${TAK_RUST_LIBRARY}"
    COMMAND "${CARGO_EXECUTABLE}" build --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
            --release --locked --offline --lib --target-dir "${TAK_CARGO_TARGET_DIR}"
    DEPENDS Cargo.toml Cargo.lock src/lib.rs src/codec.rs src/ffi.rs src/bits.rs src/header.rs src/codes.rs
            src/legacy.rs src/legacy_codes.rs
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
    COMMENT "Building the Rust TAK decoder"
    VERBATIM
)
add_custom_target(tak_rust_build DEPENDS "${TAK_RUST_LIBRARY}")
add_library(tak_rust STATIC IMPORTED)
set_target_properties(tak_rust PROPERTIES IMPORTED_LOCATION "${TAK_RUST_LIBRARY}")
add_dependencies(tak_rust tak_rust_build)

create_fooyin_plugin(tak
    JSON_IN plugin/tak.json.in
    SOURCES plugin/takplugin.cpp plugin/takplugin.h include/fooyin_tak.h
    DEPENDS Fooyin::Core tak_rust Threads::Threads ${CMAKE_DL_LIBS}
)
target_include_directories(tak PRIVATE include)
# The SDK's helper assumes fooyin's own build-directory variables. Use the
# actual imported library directory for a standalone/user-prefix installation.
if(CMAKE_INSTALL_RPATH)
    set_target_properties(tak PROPERTIES INSTALL_RPATH "${CMAKE_INSTALL_RPATH}")
else()
    set_target_properties(tak PROPERTIES INSTALL_RPATH "$<TARGET_FILE_DIR:Fooyin::Core>")
endif()
if(UNIX AND NOT APPLE)
    target_link_libraries(tak PRIVATE m)
    target_link_options(tak PRIVATE "LINKER:--exclude-libs,ALL" "LINKER:--no-undefined")
elseif(WIN32)
    # Native dependencies of Rust's Windows standard library.
    target_link_libraries(tak PRIVATE bcrypt ntdll userenv ws2_32 advapi32)
    target_compile_definitions(tak PRIVATE NOMINMAX)
endif()

file(GENERATE OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/build-info-$<CONFIG>.txt" CONTENT
"TAK plugin: ${PROJECT_VERSION}\nFooyin SDK: ${Fooyin_VERSION}\nQt: ${Qt6Core_VERSION}\nPlatform: ${CMAKE_SYSTEM_NAME} ${CMAKE_SYSTEM_PROCESSOR}\nCompiler: ${CMAKE_CXX_COMPILER_ID} ${CMAKE_CXX_COMPILER_VERSION}\nConfiguration: $<CONFIG>\n")

install(FILES LICENSE LICENSES/ffmpeg-tak.txt LICENSES/librempeg-tak.txt LICENSES/LGPL-2.1.txt
        DESTINATION "${CMAKE_INSTALL_DATADIR}/licenses/fooyin-tak")

if(BUILD_TESTING)
    add_test(NAME rust_decoder
        COMMAND "${CARGO_EXECUTABLE}" test --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
                --locked --offline --target-dir "${TAK_CARGO_TARGET_DIR}")
    add_executable(tak_plugin_test tests/plugin_test.cpp)
    target_link_libraries(tak_plugin_test PRIVATE Fooyin::Core)
    add_dependencies(tak_plugin_test tak)
    add_test(NAME plugin_integration
        COMMAND tak_plugin_test "$<TARGET_FILE:tak>" "${CMAKE_CURRENT_SOURCE_DIR}/tests/fixtures")
endif()
