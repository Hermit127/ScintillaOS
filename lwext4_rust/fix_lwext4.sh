#!/bin/bash
# lwext4 C 库修复脚本 —— 克隆完 c/lwext4 后执行

set -e
BASE=~/os_test/oskernel2026test/lwext4_rust

echo "=== 1. ext4_fs.c: 删除 *read_only = false ==="
sed -i 's/\*read_only = false;/\/\* keep caller preference \*\//' $BASE/c/lwext4/src/ext4_fs.c
grep -n "read_only" $BASE/c/lwext4/src/ext4_fs.c | tail -5

echo "=== 2. musl-generic.cmake: musl -> gnu ==="
sed -i 's/linux-musl/linux-gnu/' $BASE/c/lwext4/toolchain/musl-generic.cmake
sed -i 's/linux-musl/linux-gnu/' $BASE/c/musl-generic.cmake
grep TOOLCHAIN $BASE/c/lwext4/toolchain/musl-generic.cmake | head -2

echo "=== 3. ulibc.c: 重写 ==="
cat > $BASE/c/ulibc.c << 'CEOF'
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

__attribute__((weak)) char *__stpcpy(char *restrict d, const char *restrict s) {
    for (; (*d = *s); s++, d++);
    return d;
}
__attribute__((weak)) char *strcpy(char *restrict d, const char *restrict s) {
    __stpcpy(d, s); return d;
}
__attribute__((weak)) int strcmp(const char *l, const char *r) {
    for (; *l == *r && *l; l++, r++);
    return *(unsigned char *)l - *(unsigned char *)r;
}
__attribute__((weak)) int strncmp(const char *_l, const char *_r, size_t n) {
    const unsigned char *l = (void *)_l, *r = (void *)_r;
    if (!n--) return 0;
    for (; *l && *r && n && *l == *r; l++, r++, n--);
    return *l - *r;
}
__attribute__((weak)) void *memset(void *s, int c, size_t n) {
    unsigned char *p = (unsigned char *)s;
    while (n--) *p++ = (unsigned char)c;
    return s;
}
typedef int (*__compar_fn_t)(const void *, const void *);
typedef int (*__compar_d_fn_t)(const void *, const void *, void *);
__attribute__((weak)) void qsort_r(void *base, size_t n, size_t sz, __compar_d_fn_t cmp, void *arg) { return; }
__attribute__((weak)) void qsort(void *base, size_t n, size_t sz, __compar_fn_t cmp) { return; }
__attribute__((weak)) char *__strcpy_chk(char *d, const char *s, size_t dl) { return strcpy(d, s); }
__attribute__((weak)) void *__memcpy_chk(void *d, const void *s, size_t n, size_t dl) { return memcpy(d, s, n); }
__attribute__((weak)) void *__memset_chk(void *s, int c, size_t n, size_t bl) { return memset(s, c, n); }
__attribute__((weak)) int __printf_chk(int flag, const char *f, ...) { return 0; }
__attribute__((weak)) int fflush(FILE *f) { return 0; }
__attribute__((weak)) FILE *stdout = NULL;
__attribute__((weak)) FILE *stderr = NULL;
__attribute__((weak)) FILE *stdin = NULL;
CEOF

echo "=== 4. CMakeLists.txt: 修复 ulibc 路径 ==="
cat > $BASE/c/lwext4/src/CMakeLists.txt << 'CMEOF'

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
CMEOF

echo "=== 5. 删除 C 库 .git (作为普通文件上传) ==="
rm -rf $BASE/c/lwext4/.git

echo "=== 6. 删除 .gitmodules ==="
rm -f ~/os_test/oskernel2026test/.gitmodules

echo "=== 7. 清理旧构建, 重新编译 ==="
rm -f $BASE/c/lwext4/liblwext4-riscv64.a
rm -rf $BASE/c/lwext4/build_musl-generic

echo "=== 完成! 现在可以 cargo build ==="
