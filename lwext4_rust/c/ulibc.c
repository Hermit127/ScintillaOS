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
