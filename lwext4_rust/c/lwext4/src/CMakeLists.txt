
option(LWEXT4_BUILD_SHARED_LIB "Build shared library" OFF)

#LIBRARY
include_directories(.)
aux_source_directory(. LWEXT4_SRC)
set(M_SRC "../../ulibc.c")

if(LWEXT4_BUILD_SHARED_LIB)
  add_library(lwext4 SHARED ${LWEXT4_SRC})
else()
  add_library(lwext4 STATIC ${LWEXT4_SRC} ${M_SRC})
endif()

if  (DEFINED SIZE)
    add_custom_target(lib_size ALL DEPENDS lwext4 COMMAND ${SIZE} liblwext4.a)
else()
endif()
