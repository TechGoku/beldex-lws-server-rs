cmake_minimum_required(VERSION 3.16)

# The scan kernel: one shared library exporting only the lws_* C ABI, with
# beldex core linked in statically. Built from the pinned submodule so the
# consensus code the server parses with is exactly the node's.
#
# beldex core expects to be the top-level CMake project, so it is configured
# and built as an ExternalProject and only its static libraries are imported.
project(beldex_lws_kernel LANGUAGES C CXX)

include(ExternalProject)
find_package(PkgConfig REQUIRED)
find_package(Threads REQUIRED)
find_package(Boost REQUIRED COMPONENTS serialization program_options)
pkg_check_modules(SODIUM REQUIRED IMPORTED_TARGET libsodium>=1.0.18)

set(BELDEX_SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../external/beldex" CACHE PATH "beldex core checkout")
set(BELDEX_BINARY_DIR "${CMAKE_BINARY_DIR}/beldex-core")

# Static libraries the kernel needs, in link order, relative to the beldex build.
set(BELDEX_LIBS
  src/cryptonote_basic/libcryptonote_basic.a
  src/ringct/libringct.a
  src/ringct/libringct_basic.a
  src/device/libdevice.a
  src/common/libcommon.a
  src/crypto/libcncrypto.a
  contrib/epee/src/libepee.a
  external/randomx/librandomx.a
  external/easyloggingpp/libeasylogging.a
  external/fmt/libfmt.a)
set(BELDEX_LIB_PATHS "")
foreach(lib IN LISTS BELDEX_LIBS)
  list(APPEND BELDEX_LIB_PATHS "${BELDEX_BINARY_DIR}/${lib}")
endforeach()

ExternalProject_Add(beldex_core
  SOURCE_DIR "${BELDEX_SOURCE_DIR}"
  BINARY_DIR "${BELDEX_BINARY_DIR}"
  CMAKE_GENERATOR "${CMAKE_GENERATOR}"
  CMAKE_ARGS
    -DCMAKE_BUILD_TYPE=${CMAKE_BUILD_TYPE}
    -DCMAKE_C_COMPILER=${CMAKE_C_COMPILER}
    -DCMAKE_CXX_COMPILER=${CMAKE_CXX_COMPILER}
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    -DBUILD_TESTS=OFF
    -DBUILD_DOCUMENTATION=OFF
    -DUSE_READLINE=OFF
    -DWITH_SYSTEMD=OFF
    -DUSE_LTO=OFF
    -DUSE_DEVICE_TREZOR=OFF
  BUILD_COMMAND ${CMAKE_COMMAND} --build <BINARY_DIR> --target
    cryptonote_basic ringct ringct_basic device common cncrypto epee randomx easylogging fmt
  INSTALL_COMMAND ""
  BUILD_BYPRODUCTS ${BELDEX_LIB_PATHS}
  USES_TERMINAL_BUILD ON)

set(BELDEX_INCLUDES
  "${BELDEX_SOURCE_DIR}/src"
  "${BELDEX_SOURCE_DIR}/contrib/epee/include"
  "${BELDEX_SOURCE_DIR}/external"
  "${BELDEX_SOURCE_DIR}/external/fmt/include"
  "${BELDEX_SOURCE_DIR}/external/oxen-encoding"
  "${BELDEX_SOURCE_DIR}/external/ghc-filesystem/include"
  "${BELDEX_SOURCE_DIR}/external/easylogging++"
  "${BELDEX_SOURCE_DIR}/external/nlohmann-json/single_include"
  "${BELDEX_BINARY_DIR}"
  "${BELDEX_BINARY_DIR}/src"
  "${BELDEX_BINARY_DIR}/translations")

add_library(beldex_core_libs INTERFACE)
add_dependencies(beldex_core_libs beldex_core)
target_include_directories(beldex_core_libs INTERFACE ${BELDEX_INCLUDES})
target_compile_definitions(beldex_core_libs INTERFACE FMT_HEADER_ONLY=0)
target_link_libraries(beldex_core_libs INTERFACE
  "-Wl,--start-group" ${BELDEX_LIB_PATHS} "-Wl,--end-group"
  Boost::serialization Boost::program_options PkgConfig::SODIUM Threads::Threads ${CMAKE_DL_LIBS})

add_library(beldex_lws_kernel SHARED src/kernel.cpp)
target_include_directories(beldex_lws_kernel PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/include")
target_compile_features(beldex_lws_kernel PRIVATE cxx_std_17)
set_target_properties(beldex_lws_kernel PROPERTIES
  CXX_VISIBILITY_PRESET hidden
  VISIBILITY_INLINES_HIDDEN ON)
target_link_libraries(beldex_lws_kernel PRIVATE beldex_core_libs)
if(NOT APPLE AND NOT WIN32)
  # Keep beldex core's own symbols out of the export table, and fail the link
  # if anything is left unresolved rather than at load time in production.
  target_link_options(beldex_lws_kernel PRIVATE "LINKER:--exclude-libs,ALL" "LINKER:--no-undefined")
endif()

install(TARGETS beldex_lws_kernel LIBRARY DESTINATION lib)
install(FILES include/lws_kernel.h DESTINATION include)

option(LWS_KERNEL_TESTS "Build the kernel self-test" ON)
if(LWS_KERNEL_TESTS)
  # The test builds its fixtures with beldex core directly, then checks them
  # through the exported C ABI only.
  enable_testing()
  add_executable(lws_kernel_selftest tests/selftest.cpp)
  target_link_libraries(lws_kernel_selftest PRIVATE beldex_lws_kernel beldex_core_libs)
  target_compile_features(lws_kernel_selftest PRIVATE cxx_std_17)
  add_test(NAME lws_kernel_selftest COMMAND lws_kernel_selftest)

  # Writes the chain fixture the Rust end-to-end test replays.
  add_executable(lws_make_chain tests/make_chain.cpp)
  target_link_libraries(lws_make_chain PRIVATE beldex_core_libs)
  target_compile_features(lws_make_chain PRIVATE cxx_std_17)
endif()
