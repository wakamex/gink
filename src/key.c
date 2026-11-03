#include "key.h"

#include <SDL3/SDL.h>
#include <stdio.h>
#include <string.h>

#include "gs_secret.h"

void data_dir(const char *given, char *out, size_t size) {
    if (given) {
        size_t n = strlen(given);
        SDL_CreateDirectory(given);
        snprintf(out, size, "%s%s", given, n && (given[n - 1] == '/' || given[n - 1] == '\\') ? "" : "/");
    } else {
        char *pref = SDL_GetPrefPath("wakamex", "gink");  // (made if missing)
        snprintf(out, size, "%s", pref ? pref : "");
        SDL_free(pref);
        // The folder had the program's former name, gsignal: one found there, with no data in the
        // new one yet, is moved to the new name. Where the system will not move it (on Windows a
        // folder something else has open cannot be renamed), the program goes on using the old one
        // rather than start an empty profile.
        size_t n = strlen(out);
        char was[1100], key[1100];
        snprintf(key, sizeof key, "%sdatabase.key", out);
        if (n > 5 && !SDL_GetPathInfo(key, NULL)) {
            snprintf(was, sizeof was, "%.*sgsignal%c", (int)(n - 5), out, out[n - 1]);
            snprintf(key, sizeof key, "%sdatabase.key", was);
            char bare[1100];
            snprintf(bare, sizeof bare, "%.*s", (int)(n - 1), out);
            if (SDL_GetPathInfo(key, NULL) && SDL_RemovePath(bare)) {
                char from[1100];
                snprintf(from, sizeof from, "%.*s", (int)strlen(was) - 1, was);
                if (!SDL_RenamePath(from, bare)) snprintf(out, size, "%s", was);
            }
        }
    }
}

bool database_key(const char *dir, uint8_t key[GSC_KEY_LEN]) {
    char path[1200];
    snprintf(path, sizeof path, "%sdatabase.key", dir);
    size_t len;
    void *saved = gs_secret_load(path, &len);
    if (saved && len == GSC_KEY_LEN) return memcpy(key, saved, len), SDL_free(saved), true;
    SDL_free(saved);
    return gsc_key_new(key) == 0 && gs_secret_save(path, key, GSC_KEY_LEN);
}
