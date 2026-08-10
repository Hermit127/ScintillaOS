#ifndef LWEXT4_FREESTANDING_STDLIB_H
#define LWEXT4_FREESTANDING_STDLIB_H

#include <stddef.h>

typedef int (*__compar_fn_t)(const void *, const void *);
typedef int (*__compar_d_fn_t)(const void *, const void *, void *);

void *malloc(size_t size);
void *calloc(size_t count, size_t size);
void *realloc(void *ptr, size_t size);
void free(void *ptr);
void qsort(void *base, size_t count, size_t size, __compar_fn_t compare);
void qsort_r(void *base, size_t count, size_t size,
             __compar_d_fn_t compare, void *arg);

#endif
