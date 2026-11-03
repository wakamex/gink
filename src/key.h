// The data folder and the database key kept in it.
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "gsignal_core.h"

// The folder gsignal keeps its data in, ending with a path separator: `given` when not NULL (made if
// missing), otherwise the system's folder for this app.
void data_dir(const char *given, char *out, size_t size);

// The database key: made by the core on the first run and kept as a secret for this user in dir.
bool database_key(const char *dir, uint8_t key[GSC_KEY_LEN]);
