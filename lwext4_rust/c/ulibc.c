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

struct sort_context {
    __compar_fn_t cmp;
    __compar_d_fn_t cmp_r;
    void *arg;
};

static int sort_compare(const struct sort_context *ctx,
                        const void *left, const void *right) {
    if (ctx->cmp_r)
        return ctx->cmp_r(left, right, ctx->arg);
    return ctx->cmp(left, right);
}

static void sort_swap(unsigned char *left, unsigned char *right, size_t size) {
    while (size--) {
        unsigned char tmp = *left;
        *left++ = *right;
        *right++ = tmp;
    }
}

static void sort_sift_down(unsigned char *base, size_t start, size_t count,
                           size_t size, const struct sort_context *ctx) {
    size_t root = start;

    while (count > 1 && root <= (count - 2) / 2) {
        size_t child = root * 2 + 1;
        size_t candidate = root;

        if (sort_compare(ctx, base + candidate * size,
                         base + child * size) < 0)
            candidate = child;
        if (child + 1 < count &&
            sort_compare(ctx, base + candidate * size,
                         base + (child + 1) * size) < 0)
            candidate = child + 1;
        if (candidate == root)
            return;

        sort_swap(base + root * size, base + candidate * size, size);
        root = candidate;
    }
}

static void sort_heap(void *base, size_t count, size_t size,
                      const struct sort_context *ctx) {
    unsigned char *items = base;
    size_t start;
    size_t end;

    if (!base || !ctx || (!ctx->cmp && !ctx->cmp_r) || count < 2 || size == 0)
        return;

    start = (count - 2) / 2 + 1;
    while (start > 0) {
        start--;
        sort_sift_down(items, start, count, size, ctx);
    }

    end = count - 1;
    while (end > 0) {
        sort_swap(items, items + end * size, size);
        sort_sift_down(items, 0, end, size, ctx);
        end--;
    }
}

__attribute__((weak)) void qsort_r(void *base, size_t n, size_t sz,
                                   __compar_d_fn_t cmp, void *arg) {
    const struct sort_context ctx = { .cmp = NULL, .cmp_r = cmp, .arg = arg };
    sort_heap(base, n, sz, &ctx);
}

__attribute__((weak)) void qsort(void *base, size_t n, size_t sz,
                                 __compar_fn_t cmp) {
    const struct sort_context ctx = { .cmp = cmp, .cmp_r = NULL, .arg = NULL };
    sort_heap(base, n, sz, &ctx);
}
__attribute__((weak)) char *__strcpy_chk(char *d, const char *s, size_t dl) { return strcpy(d, s); }
__attribute__((weak)) void *__memcpy_chk(void *d, const void *s, size_t n, size_t dl) { return memcpy(d, s, n); }
__attribute__((weak)) void *__memset_chk(void *s, int c, size_t n, size_t bl) { return memset(s, c, n); }
__attribute__((weak)) int __printf_chk(int flag, const char *f, ...) { return 0; }
__attribute__((weak)) int fflush(FILE *f) { return 0; }
__attribute__((weak)) FILE *stdout = NULL;
__attribute__((weak)) FILE *stderr = NULL;
__attribute__((weak)) FILE *stdin = NULL;
