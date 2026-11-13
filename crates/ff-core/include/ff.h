/* ff C API. Link with libff_core.a (fully static, no other deps).
 *
 * All functions accept NULL handles and then return NULL / 0 / -1.
 * Strings are NUL-terminated byte strings; paths need not be UTF-8.
 * An index may be searched from several threads at once.
 */
#ifndef FF_H
#define FF_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct FfIndex FfIndex;
typedef struct FfResults FfResults;

enum { FF_KIND_ANY = 0, FF_KIND_FILE = 1, FF_KIND_DIR = 2 };
enum { FF_SORT_NONE = 0, FF_SORT_NAME = 1, FF_SORT_PATH = 2, FF_SORT_SIZE = 3, FF_SORT_MTIME = 4 };

/* Walk `roots` and build an index. `excludes` (may be NULL) are directories
 * to skip, in addition to /proc, /sys, /dev, /run and /.snapshots.
 * Returns NULL on error (e.g. a root that is not a directory). */
FfIndex *ff_index_build(const char *const *roots, size_t n_roots,
                        const char *const *excludes, size_t n_excludes);

/* Load / save an index file. Save returns 0 on success, -1 on error. */
FfIndex *ff_index_load(const char *path);
int ff_index_save(const FfIndex *index, const char *path);

size_t ff_index_len(const FfIndex *index);
void ff_index_free(FfIndex *index);

/* Run a query. Terms are whitespace-separated and ANDed:
 *   foo        name contains "foo" (case-insensitive unless it has uppercase)
 *   *.pdf      wildcard on the whole name
 *   src/main   contains '/': matched against the full path
 *   !foo       negation
 *   "my file"  quotes keep spaces inside a term
 * `limit` 0 means unlimited. The results borrow `index`: free them first. */
FfResults *ff_search(const FfIndex *index, const char *query,
                     int kind, int sort, int reverse, size_t limit);

size_t ff_results_len(const FfResults *results);

/* Copy the path of result `i` into `buf` (NUL-terminated, truncated to `cap`).
 * Returns the full path length, so a return >= cap means truncation. */
size_t ff_results_path(const FfResults *results, size_t i, char *buf, size_t cap);

uint64_t ff_results_size(const FfResults *results, size_t i);  /* bytes */
int64_t ff_results_mtime(const FfResults *results, size_t i);  /* Unix seconds */
int ff_results_is_dir(const FfResults *results, size_t i);
void ff_results_free(FfResults *results);

#ifdef __cplusplus
}
#endif

#endif /* FF_H */
