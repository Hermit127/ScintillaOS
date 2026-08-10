#ifndef LWEXT4_FREESTANDING_STRING_H
#define LWEXT4_FREESTANDING_STRING_H

#include <stddef.h>

void *memcpy(void *restrict dest, const void *restrict src, size_t count);
void *memmove(void *dest, const void *src, size_t count);
void *memset(void *dest, int value, size_t count);
int memcmp(const void *left, const void *right, size_t count);
size_t strlen(const char *str);
char *strcpy(char *restrict dest, const char *restrict src);
char *strncpy(char *restrict dest, const char *restrict src, size_t count);
int strcmp(const char *left, const char *right);
int strncmp(const char *left, const char *right, size_t count);

#endif
