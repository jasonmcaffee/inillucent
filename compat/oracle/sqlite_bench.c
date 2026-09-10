/*
** The SQLite side of the inillucent performance scorecard.
**
** Invariant: this program is handed the *same plan file* the inillucent side
** reads, and every statement it runs is a byte-for-byte copy of what the other
** engine runs. The fairness contract is not a promise in a document here; it is
** that there is one copy of the SQL, one copy of the parameter generator, and
** one copy of the transaction grouping, and both engines are driven from them.
**
** It is compiled from the pinned SQLite 3.53.4 amalgamation and run as a
** separate child process, which is the only way SQLite appears in this
** workspace. No inillucent crate links against it.
**
** Usage:
**   sqlite-bench build <plan> <database>
**   sqlite-bench run   <plan> <database>
**
** `build` creates the pristine database from the plan's setup statements.
** `run` executes every workload in plan order and writes one line per workload
** to stdout:
**
**   <name>\t<nanos>\t<rows>\t<digest>
**
** The digest is the correctness qualification: a workload whose two engines
** produce different digests is not a performance sample, it is a bug, and the
** analyser refuses to time it.
*/

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include "sqlite3.h"

#if defined(_WIN32)
#include <windows.h>
static double now_seconds(void) {
  static LARGE_INTEGER frequency;
  LARGE_INTEGER counter;
  if (frequency.QuadPart == 0) QueryPerformanceFrequency(&frequency);
  QueryPerformanceCounter(&counter);
  return (double)counter.QuadPart / (double)frequency.QuadPart;
}
#else
#include <time.h>
static double now_seconds(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}
#endif

#define MAX_LINE 8192
#define MAX_SQL 8192
#define MAX_BINDS 8
#define MAX_WORKLOADS 64
#define MAX_SETUP 64

/* The parameter generators, which must match the inillucent side exactly. */
enum bind_kind {
  BIND_NONE = 0,
  BIND_ROWID,   /* 1 + (iteration % rows) */
  BIND_SCATTER, /* 1 + ((iteration * 2654435761) % rows) */
  BIND_COUNTER, /* rows + 1 + iteration */
  BIND_INT,     /* (iteration * 1103515245 + 12345) & 0x7fffffff */
  BIND_TEXT,    /* "row <iteration> lorem ipsum dolor sit amet consectetur" */
  BIND_BLOB     /* 64 bytes, byte j = (iteration + j) & 0xff */
};

struct workload {
  char name[128];
  char family[64];
  char sql[MAX_SQL];
  char pre[MAX_SQL];
  char post[MAX_SQL];
  int repeat;
  int txn;        /* 0 autocommit, -1 one transaction, n commit every n */
  int prepare_each;
  int binds[MAX_BINDS];
  int bind_count;
};

struct plan {
  long rows;
  char journal[32];
  char locking[32];
  char synchronous[32];
  long page_size;
  long cache_size;
  char setup[MAX_SETUP][MAX_SQL];
  int setup_count;
  struct workload workloads[MAX_WORKLOADS];
  int workload_count;
};

/* ------------------------------------------------------------------------ */
/* The digest, which has to agree byte for byte with the inillucent side.       */
/* ------------------------------------------------------------------------ */

static uint64_t digest_start(void) { return 0xcbf29ce484222325ULL; }

static void digest_bytes(uint64_t *hash, const unsigned char *bytes, size_t length) {
  size_t i;
  for (i = 0; i < length; i++) {
    *hash ^= (uint64_t)bytes[i];
    *hash *= 0x100000001b3ULL;
  }
}

static void digest_tag(uint64_t *hash, unsigned char tag) { digest_bytes(hash, &tag, 1); }

static void digest_u64(uint64_t *hash, uint64_t value) {
  unsigned char bytes[8];
  int i;
  for (i = 0; i < 8; i++) bytes[i] = (unsigned char)((value >> (8 * i)) & 0xff);
  digest_bytes(hash, bytes, 8);
}

static void digest_value(uint64_t *hash, sqlite3_stmt *statement, int column) {
  switch (sqlite3_column_type(statement, column)) {
    case SQLITE_NULL:
      digest_tag(hash, 0);
      break;
    case SQLITE_INTEGER:
      digest_tag(hash, 1);
      digest_u64(hash, (uint64_t)sqlite3_column_int64(statement, column));
      break;
    case SQLITE_FLOAT: {
      double value = sqlite3_column_double(statement, column);
      uint64_t bits;
      memcpy(&bits, &value, sizeof(bits));
      digest_tag(hash, 2);
      digest_u64(hash, bits);
      break;
    }
    case SQLITE_TEXT: {
      const unsigned char *text = sqlite3_column_text(statement, column);
      int length = sqlite3_column_bytes(statement, column);
      digest_tag(hash, 3);
      digest_u64(hash, (uint64_t)length);
      if (text) digest_bytes(hash, text, (size_t)length);
      break;
    }
    default: {
      const void *blob = sqlite3_column_blob(statement, column);
      int length = sqlite3_column_bytes(statement, column);
      digest_tag(hash, 4);
      digest_u64(hash, (uint64_t)length);
      if (blob) digest_bytes(hash, (const unsigned char *)blob, (size_t)length);
      break;
    }
  }
}

/* ------------------------------------------------------------------------ */
/* Reading the plan.                                                        */
/* ------------------------------------------------------------------------ */

static int bind_kind_of(const char *name) {
  if (strcmp(name, "rowid") == 0) return BIND_ROWID;
  if (strcmp(name, "scatter") == 0) return BIND_SCATTER;
  if (strcmp(name, "counter") == 0) return BIND_COUNTER;
  if (strcmp(name, "int") == 0) return BIND_INT;
  if (strcmp(name, "text") == 0) return BIND_TEXT;
  if (strcmp(name, "blob") == 0) return BIND_BLOB;
  return BIND_NONE;
}

static void trim_newline(char *line) {
  size_t length = strlen(line);
  while (length > 0 && (line[length - 1] == '\n' || line[length - 1] == '\r')) {
    line[--length] = 0;
  }
}

static int read_plan(const char *path, struct plan *plan) {
  FILE *file = fopen(path, "rb");
  char line[MAX_LINE];
  struct workload *current = 0;
  if (!file) {
    fprintf(stderr, "cannot open %s\n", path);
    return 0;
  }
  memset(plan, 0, sizeof(*plan));
  plan->page_size = 4096;
  plan->cache_size = -2000;
  strcpy(plan->journal, "delete");
  strcpy(plan->locking, "normal");
  strcpy(plan->synchronous, "full");
  while (fgets(line, sizeof(line), file)) {
    char *tab;
    char *key;
    char *value;
    trim_newline(line);
    if (line[0] == 0 || line[0] == '#') continue;
    tab = strchr(line, '\t');
    if (!tab) continue;
    *tab = 0;
    key = line;
    value = tab + 1;
    if (strcmp(key, "rows") == 0) {
      plan->rows = strtol(value, 0, 10);
    } else if (strcmp(key, "journal") == 0) {
      snprintf(plan->journal, sizeof(plan->journal), "%s", value);
    } else if (strcmp(key, "locking") == 0) {
      snprintf(plan->locking, sizeof(plan->locking), "%s", value);
    } else if (strcmp(key, "synchronous") == 0) {
      snprintf(plan->synchronous, sizeof(plan->synchronous), "%s", value);
    } else if (strcmp(key, "page_size") == 0) {
      plan->page_size = strtol(value, 0, 10);
    } else if (strcmp(key, "cache_size") == 0) {
      plan->cache_size = strtol(value, 0, 10);
    } else if (strcmp(key, "setup") == 0) {
      if (plan->setup_count < MAX_SETUP) {
        snprintf(plan->setup[plan->setup_count], MAX_SQL, "%s", value);
        plan->setup_count++;
      }
    } else if (strcmp(key, "workload") == 0) {
      if (plan->workload_count >= MAX_WORKLOADS) break;
      current = &plan->workloads[plan->workload_count++];
      memset(current, 0, sizeof(*current));
      snprintf(current->name, sizeof(current->name), "%s", value);
      current->repeat = 1;
    } else if (current) {
      if (strcmp(key, "family") == 0) {
        snprintf(current->family, sizeof(current->family), "%s", value);
      } else if (strcmp(key, "sql") == 0) {
        snprintf(current->sql, MAX_SQL, "%s", value);
      } else if (strcmp(key, "pre") == 0) {
        snprintf(current->pre, MAX_SQL, "%s", value);
      } else if (strcmp(key, "post") == 0) {
        snprintf(current->post, MAX_SQL, "%s", value);
      } else if (strcmp(key, "repeat") == 0) {
        current->repeat = (int)strtol(value, 0, 10);
      } else if (strcmp(key, "txn") == 0) {
        if (strcmp(value, "all") == 0) {
          current->txn = -1;
        } else if (strcmp(value, "none") == 0) {
          current->txn = 0;
        } else {
          current->txn = (int)strtol(value, 0, 10);
        }
      } else if (strcmp(key, "prepare") == 0) {
        current->prepare_each = strcmp(value, "each") == 0;
      } else if (strcmp(key, "bind") == 0) {
        char buffer[256];
        char *token;
        snprintf(buffer, sizeof(buffer), "%s", value);
        token = strtok(buffer, ",");
        while (token && current->bind_count < MAX_BINDS) {
          current->binds[current->bind_count++] = bind_kind_of(token);
          token = strtok(0, ",");
        }
      }
    }
  }
  fclose(file);
  return 1;
}

/* ------------------------------------------------------------------------ */
/* Running.                                                                 */
/* ------------------------------------------------------------------------ */

static int fail(sqlite3 *db, const char *what) {
  fprintf(stderr, "%s: %s\n", what, db ? sqlite3_errmsg(db) : "no database");
  return 0;
}

static int run_sql(sqlite3 *db, const char *sql) {
  char *message = 0;
  if (sqlite3_exec(db, sql, 0, 0, &message) != SQLITE_OK) {
    fprintf(stderr, "%s: %s\n", sql, message ? message : "failed");
    sqlite3_free(message);
    return 0;
  }
  return 1;
}

static void bind_one(sqlite3_stmt *statement, int position, int kind, long iteration, long rows) {
  char text[128];
  unsigned char blob[64];
  int j;
  long value;
  switch (kind) {
    case BIND_ROWID:
      value = rows > 0 ? 1 + (iteration % rows) : 1;
      sqlite3_bind_int64(statement, position, value);
      break;
    case BIND_SCATTER:
      value = rows > 0 ? (long)(1 + (((uint64_t)iteration * 2654435761ULL) % (uint64_t)rows)) : 1;
      sqlite3_bind_int64(statement, position, value);
      break;
    case BIND_COUNTER:
      sqlite3_bind_int64(statement, position, rows + 1 + iteration);
      break;
    case BIND_INT:
      value = (long)((((uint64_t)iteration * 1103515245ULL) + 12345ULL) & 0x7fffffffULL);
      sqlite3_bind_int64(statement, position, value);
      break;
    case BIND_TEXT:
      snprintf(text, sizeof(text), "row %ld lorem ipsum dolor sit amet consectetur", iteration);
      sqlite3_bind_text(statement, position, text, -1, SQLITE_TRANSIENT);
      break;
    case BIND_BLOB:
      for (j = 0; j < 64; j++) blob[j] = (unsigned char)((iteration + j) & 0xff);
      sqlite3_bind_blob(statement, position, blob, 64, SQLITE_TRANSIENT);
      break;
    default:
      break;
  }
}

static int run_workload(sqlite3 *db, const struct workload *workload, long rows) {
  sqlite3_stmt *statement = 0;
  uint64_t hash = digest_start();
  long produced = 0;
  long iteration;
  double started;
  double elapsed;
  if (workload->pre[0] && !run_sql(db, workload->pre)) return 0;
  if (!workload->prepare_each) {
    if (sqlite3_prepare_v2(db, workload->sql, -1, &statement, 0) != SQLITE_OK) {
      return fail(db, workload->sql);
    }
  }
  started = now_seconds();
  if (workload->txn != 0 && !run_sql(db, "BEGIN")) return 0;
  for (iteration = 0; iteration < workload->repeat; iteration++) {
    int position;
    int status;
    if (workload->prepare_each) {
      if (sqlite3_prepare_v2(db, workload->sql, -1, &statement, 0) != SQLITE_OK) {
        return fail(db, workload->sql);
      }
    }
    for (position = 0; position < workload->bind_count; position++) {
      bind_one(statement, position + 1, workload->binds[position], iteration, rows);
    }
    while ((status = sqlite3_step(statement)) == SQLITE_ROW) {
      int column;
      int columns = sqlite3_column_count(statement);
      for (column = 0; column < columns; column++) digest_value(&hash, statement, column);
      produced++;
    }
    if (status != SQLITE_DONE) return fail(db, workload->sql);
    if (workload->prepare_each) {
      sqlite3_finalize(statement);
      statement = 0;
    } else {
      sqlite3_reset(statement);
      sqlite3_clear_bindings(statement);
    }
    if (workload->txn > 0 && ((iteration + 1) % workload->txn) == 0) {
      if (!run_sql(db, "COMMIT")) return 0;
      if (iteration + 1 < workload->repeat && !run_sql(db, "BEGIN")) return 0;
    }
  }
  if (workload->txn < 0) {
    if (!run_sql(db, "COMMIT")) return 0;
  } else if (workload->txn > 0 && (workload->repeat % workload->txn) != 0) {
    if (!run_sql(db, "COMMIT")) return 0;
  }
  elapsed = now_seconds() - started;
  if (statement) sqlite3_finalize(statement);
  if (workload->post[0] && !run_sql(db, workload->post)) return 0;
  printf("%s\t%.0f\t%ld\t%016llx\n", workload->name, elapsed * 1e9, produced,
         (unsigned long long)hash);
  return 1;
}

static int apply_settings(sqlite3 *db, const struct plan *plan) {
  char sql[256];
  snprintf(sql, sizeof(sql), "PRAGMA page_size=%ld;", plan->page_size);
  if (!run_sql(db, sql)) return 0;
  /*
  ** Before journal_mode, because changing the locking mode after a journal
  ** mode has been set is refused while a lock is held. `normal` is the default
  ** and is what every gate used to run; `exclusive` is what the new
  ** engine actually does - one process, no lock taken per statement - and the
  ** gate can now ask for either so the difference is a number rather than an
  ** argument.
  */
  snprintf(sql, sizeof(sql), "PRAGMA locking_mode=%s;", plan->locking);
  if (!run_sql(db, sql)) return 0;
  snprintf(sql, sizeof(sql), "PRAGMA journal_mode=%s;", plan->journal);
  if (!run_sql(db, sql)) return 0;
  snprintf(sql, sizeof(sql), "PRAGMA synchronous=%s;", plan->synchronous);
  if (!run_sql(db, sql)) return 0;
  snprintf(sql, sizeof(sql), "PRAGMA cache_size=%ld;", plan->cache_size);
  if (!run_sql(db, sql)) return 0;
  return 1;
}

/*
** The plan is static rather than automatic. It holds a fixed-size slot for
** every statement of every workload, which is a couple of megabytes - and the
** default stack on Windows is one. A structure this size in `main` compiles
** cleanly and then fails at run time with a stack overflow and no message,
** which is the least helpful failure a benchmark driver could have.
*/
static struct plan the_plan;

int main(int argc, char **argv) {
  struct plan *plan = &the_plan;
  sqlite3 *db = 0;
  int index;
  if (argc < 4) {
    fprintf(stderr, "usage: sqlite-bench build|run <plan> <database>\n");
    return 2;
  }
  if (!read_plan(argv[2], plan)) return 2;
  if (sqlite3_open(argv[3], &db) != SQLITE_OK) {
    fail(db, "open");
    return 2;
  }
  if (!apply_settings(db, plan)) return 2;
  if (strcmp(argv[1], "build") == 0) {
    for (index = 0; index < plan->setup_count; index++) {
      if (!run_sql(db, plan->setup[index])) return 2;
    }
    sqlite3_close(db);
    return 0;
  }
  for (index = 0; index < plan->workload_count; index++) {
    if (!run_workload(db, &plan->workloads[index], plan->rows)) return 2;
  }
  sqlite3_close(db);
  return 0;
}
