/*
 * Loaded into a Player with LD_PRELOAD: a native plugin of the game that the
 * Player opens from its own data (`<Player>_Data/Plugins/...`) is opened with
 * RTLD_DEEPBIND, so that it binds to its own symbols before those already
 * loaded. Firebase's desktop library and another gRPC of the game (or of the
 * engine) each carry gRPC and Abseil; without it the second one's static
 * initializers run on the first one's globals and crash the process.
 *
 * UME_DEEPBIND_PREFIXES names the directories whose libraries are opened so,
 * separated by ':'. UME_DEEPBIND_LOG prints every open on standard error.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <link.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef void *(*open_function)(const char *, int);

static open_function next_open;

static int deep(const char *path)
{
    const char *prefixes = getenv("UME_DEEPBIND_PREFIXES");
    if (path == NULL || prefixes == NULL || strstr(path, ".so") == NULL)
        return 0;
    for (const char *start = prefixes; *start != '\0';) {
        const char *end = strchr(start, ':');
        size_t length = end == NULL ? strlen(start) : (size_t)(end - start);
        if (length > 0 && strncmp(path, start, length) == 0)
            return 1;
        if (end == NULL)
            break;
        start = end + 1;
    }
    return 0;
}

static void *ume_open(const char *path, int flags)
{
    if (next_open == NULL)
        next_open = (open_function)dlvsym(RTLD_NEXT, "dlopen", "GLIBC_2.34");
    if (next_open == NULL)
        next_open = (open_function)dlvsym(RTLD_NEXT, "dlopen", "GLIBC_2.2.5");
    if (deep(path))
        flags |= RTLD_DEEPBIND;
    void *handle = next_open(path, flags);
    if (getenv("UME_DEEPBIND_LOG") != NULL) {
        struct link_map *map = NULL;
        if (handle != NULL)
            dlinfo(handle, RTLD_DI_LINKMAP, &map);
        fprintf(stderr, "ume-deepbind: %s flags=%#x -> %p base=%#lx\n", path ? path : "(null)", flags, handle,
                map != NULL ? (unsigned long)map->l_addr : 0UL);
    }
    return handle;
}

void *dlopen(const char *path, int flags)
{
    return ume_open(path, flags);
}
