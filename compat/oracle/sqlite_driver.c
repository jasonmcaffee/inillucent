/*
** The SQLite side of the inillucent oracle protocol.
**
** Invariant: this program observes SQLite through its public C API and reports
** exactly what it saw. It never normalises, rounds, or re-encodes a value: an
** integer leaves as its big-endian bytes, a double as its IEEE-754 bits, and
** text and blobs as their bytes. A harness that compared decimal renderings
** would be comparing its own formatting rather than the two engines.
**
** It is compiled from the pinned SQLite 3.53.4 amalgamation and run as a
** separate child process, which is the only way SQLite is allowed to appear in
** this workspace. No inillucent crate links against it.
**
** Protocol: one JSON object per line on stdin, one per line on stdout.
**
**   {"op":"hello"}                          -> {"ok":true,"driver":"sqlite",...}
**   {"op":"open","path":"..."}              -> {"ok":true} | error
**   {"op":"exec","sql":"..."}               -> {"ok":true,"changes":n,...}
**   {"op":"query","sql":"..."}              -> {"ok":true,"columns":[...],"rows":[[...]]}
**   {"op":"echo","values":[{...},...]}      -> {"ok":true,"rows":[[...]]}
**   {"op":"bind","sql":"...","values":[...]} -> {"ok":true,"rows":[[...]]}
**   {"op":"limits"}                         -> {"ok":true,"rows":[[name,value],...]}
**   {"op":"close"}                          -> {"ok":true}
**   {"op":"bye"}                            -> (exits)
*/

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include "sqlite3.h"

#define MAX_LINE (1 << 22)
#define MAX_BIND 64

static sqlite3 *db = 0;

/* ------------------------------------------------------------------------ */
/* Minimal JSON reading for the flat shapes this protocol uses.              */
/* ------------------------------------------------------------------------ */

/* Returns a pointer just past "key": in line, or NULL when it is absent. */
static const char *field(const char *line, const char *key){
  char needle[64];
  const char *found;
  snprintf(needle, sizeof(needle), "\"%s\":", key);
  found = strstr(line, needle);
  if( found==0 ) return 0;
  found += strlen(needle);
  while( *found==' ' ) found++;
  return found;
}

/*
** Copies the string value of a field into out, undoing the escapes the Rust
** side emits. Returns 1 on success.
*/
static int string_field(const char *line, const char *key, char *out, size_t cap){
  const char *at = field(line, key);
  size_t used = 0;
  if( at==0 || *at!='"' ) return 0;
  at++;
  while( *at && used+1<cap ){
    if( *at=='"' ){ out[used] = 0; return 1; }
    if( *at=='\\' ){
      at++;
      switch( *at ){
        case 'n': out[used++] = '\n'; break;
        case 't': out[used++] = '\t'; break;
        case 'r': out[used++] = '\r'; break;
        case 0:   out[used] = 0; return 0;
        default:  out[used++] = *at; break;
      }
      at++;
      continue;
    }
    out[used++] = *at++;
  }
  out[used] = 0;
  return 0;
}

/* Converts one hex digit, or -1. */
static int hex_digit(char c){
  if( c>='0' && c<='9' ) return c - '0';
  if( c>='a' && c<='f' ) return c - 'a' + 10;
  if( c>='A' && c<='F' ) return c - 'A' + 10;
  return -1;
}

/*
** Decodes a hex string into out. Returns the byte count, or -1 when the input
** is not hex or does not fit.
*/
static int decode_hex(const char *text, size_t length, unsigned char *out, size_t cap){
  size_t index;
  if( length % 2 ) return -1;
  if( length/2 > cap ) return -1;
  for(index=0; index<length; index+=2){
    int high = hex_digit(text[index]);
    int low = hex_digit(text[index+1]);
    if( high<0 || low<0 ) return -1;
    out[index/2] = (unsigned char)((high<<4) | low);
  }
  return (int)(length/2);
}

/* ------------------------------------------------------------------------ */
/* JSON writing.                                                            */
/* ------------------------------------------------------------------------ */

/* Writes a JSON string literal for text. */
static void put_json_string(const char *text){
  const unsigned char *at = (const unsigned char *)text;
  putchar('"');
  for(; *at; at++){
    switch( *at ){
      case '"':  fputs("\\\"", stdout); break;
      case '\\': fputs("\\\\", stdout); break;
      case '\n': fputs("\\n", stdout); break;
      case '\r': fputs("\\r", stdout); break;
      case '\t': fputs("\\t", stdout); break;
      default:
        if( *at < 0x20 ) printf("\\u%04x", *at);
        else putchar(*at);
    }
  }
  putchar('"');
}

/* Writes bytes as lowercase hex. */
static void put_hex(const unsigned char *bytes, int length){
  int index;
  for(index=0; index<length; index++) printf("%02x", bytes[index]);
}

/* Writes one column of the current row as a tagged value. */
static void put_column(sqlite3_stmt *statement, int column){
  switch( sqlite3_column_type(statement, column) ){
    case SQLITE_NULL:
      fputs("{\"class\":\"null\"}", stdout);
      break;
    case SQLITE_INTEGER: {
      sqlite3_int64 value = sqlite3_column_int64(statement, column);
      unsigned char bytes[8];
      int index;
      for(index=0; index<8; index++){
        bytes[index] = (unsigned char)((value >> (8*(7-index))) & 0xff);
      }
      fputs("{\"class\":\"integer\",\"be_hex\":\"", stdout);
      put_hex(bytes, 8);
      fputs("\"}", stdout);
      break;
    }
    case SQLITE_FLOAT: {
      double value = sqlite3_column_double(statement, column);
      unsigned char bytes[8];
      sqlite3_uint64 bits;
      int index;
      memcpy(&bits, &value, 8);
      for(index=0; index<8; index++){
        bytes[index] = (unsigned char)((bits >> (8*(7-index))) & 0xff);
      }
      fputs("{\"class\":\"real\",\"ieee754_hex\":\"", stdout);
      put_hex(bytes, 8);
      fputs("\"}", stdout);
      break;
    }
    case SQLITE_TEXT: {
      const unsigned char *body = sqlite3_column_text(statement, column);
      int length = sqlite3_column_bytes(statement, column);
      fputs("{\"class\":\"text\",\"utf8_hex\":\"", stdout);
      if( body ) put_hex(body, length);
      fputs("\"}", stdout);
      break;
    }
    default: {
      const unsigned char *body = sqlite3_column_blob(statement, column);
      int length = sqlite3_column_bytes(statement, column);
      fputs("{\"class\":\"blob\",\"hex\":\"", stdout);
      if( body ) put_hex(body, length);
      fputs("\"}", stdout);
      break;
    }
  }
}

/* Writes the trailing connection state every successful reply carries. */
static void put_state(void){
  printf(",\"changes\":%d,\"total_changes\":%d,\"last_insert_rowid\":%lld,\"autocommit\":%s",
         db ? sqlite3_changes(db) : 0,
         db ? sqlite3_total_changes(db) : 0,
         db ? (long long)sqlite3_last_insert_rowid(db) : 0,
         (db==0 || sqlite3_get_autocommit(db)) ? "true" : "false");
}

/* Writes an error reply carrying both result codes and the message. */
static void put_error(int code, const char *message){
  printf("{\"ok\":false,\"code\":%d,\"extended\":%d,\"message\":", code & 0xff, code);
  put_json_string(message ? message : "");
  put_state();
  fputs("}\n", stdout);
  fflush(stdout);
}

/* Writes the error the open connection is currently reporting. */
static void put_db_error(int code){
  put_error(code, db ? sqlite3_errmsg(db) : "no database is open");
}

/* ------------------------------------------------------------------------ */
/* Commands.                                                                */
/* ------------------------------------------------------------------------ */

/* Opens a database, closing whatever was open before. */
static void do_open(const char *line){
  static char path[4096];
  int rc;
  if( !string_field(line, "path", path, sizeof(path)) ){
    put_error(SQLITE_MISUSE, "open needs a path");
    return;
  }
  if( db ){ sqlite3_close(db); db = 0; }
  rc = sqlite3_open(path, &db);
  if( rc!=SQLITE_OK ){
    put_db_error(rc);
    if( db ){ sqlite3_close(db); db = 0; }
    return;
  }
  fputs("{\"ok\":true", stdout);
  put_state();
  fputs("}\n", stdout);
  fflush(stdout);
}

/* Runs SQL that is not expected to return rows. */
static void do_exec(const char *sql){
  char *message = 0;
  int rc;
  if( db==0 ){ put_error(SQLITE_MISUSE, "no database is open"); return; }
  rc = sqlite3_exec(db, sql, 0, 0, &message);
  if( rc!=SQLITE_OK ){
    put_error(sqlite3_extended_errcode(db), message ? message : sqlite3_errmsg(db));
    sqlite3_free(message);
    return;
  }
  sqlite3_free(message);
  fputs("{\"ok\":true", stdout);
  put_state();
  fputs("}\n", stdout);
  fflush(stdout);
}

/* Runs SQL and reports its columns and rows as tagged values. */
static void do_query(const char *sql){
  sqlite3_stmt *statement = 0;
  int rc, column, columns, first_row = 1;
  if( db==0 ){ put_error(SQLITE_MISUSE, "no database is open"); return; }
  rc = sqlite3_prepare_v2(db, sql, -1, &statement, 0);
  if( rc!=SQLITE_OK ){ put_db_error(sqlite3_extended_errcode(db)); return; }
  columns = sqlite3_column_count(statement);
  /* "ok" is written *after* the rows, because a statement can fail partway
  ** through stepping - PRAGMA integrity_check does exactly that on a damaged
  ** file - and a reply that had already claimed success would then need a
  ** second line to take it back. Two lines for one command desynchronises the
  ** protocol for everything that follows, which is worse than the error. */
  fputs("{\"columns\":[", stdout);
  for(column=0; column<columns; column++){
    if( column ) putchar(',');
    put_json_string(sqlite3_column_name(statement, column));
  }
  fputs("],\"rows\":[", stdout);
  while( (rc = sqlite3_step(statement))==SQLITE_ROW ){
    if( !first_row ) putchar(',');
    first_row = 0;
    putchar('[');
    for(column=0; column<columns; column++){
      if( column ) putchar(',');
      put_column(statement, column);
    }
    putchar(']');
  }
  fputs("]", stdout);
  if( rc!=SQLITE_DONE ){
    int code = sqlite3_extended_errcode(db);
    const char *message = sqlite3_errmsg(db);
    printf(",\"ok\":false,\"code\":%d,\"extended\":%d,\"message\":", code & 0xff, code);
    put_json_string(message);
    sqlite3_finalize(statement);
    fputs("}\n", stdout);
    fflush(stdout);
    return;
  }
  fputs(",\"ok\":true", stdout);
  sqlite3_finalize(statement);
  put_state();
  fputs("}\n", stdout);
  fflush(stdout);
}

/*
** Binds the values of an echo command into SELECT ?1, ?2, ... and reads them
** back. This is what proves the protocol carries every storage class without
** loss: the values go through SQLite's own value system, not around it.
*/
static void do_bound(const char *line, const char *explicit_sql){
  static unsigned char payload[1 << 20];
  static char generated[MAX_BIND * 8 + 32];
  const char *sql = explicit_sql;
  const char *at = field(line, "values");
  sqlite3_stmt *statement = 0;
  int count = 0, rc, column, columns;
  int owned = 0;
  if( db==0 ){
    rc = sqlite3_open(":memory:", &db);
    if( rc!=SQLITE_OK ){ put_db_error(rc); return; }
    owned = 1;
  }
  if( at==0 ){ put_error(SQLITE_MISUSE, "a bound query needs a values array"); return; }

  /* Count the value objects so the SELECT can be built with the right arity. */
  {
    const char *scan = at;
    while( *scan && *scan!=']' ){
      if( *scan=='{' ) count++;
      scan++;
    }
  }
  if( count==0 || count>MAX_BIND ){
    put_error(SQLITE_MISUSE, "a bound query takes between one and 64 values");
    return;
  }
  if( sql==0 ){
    int index;
    strcpy(generated, "SELECT ");
    for(index=0; index<count; index++){
      char parameter[12];
      snprintf(parameter, sizeof(parameter), "%s?%d", index ? "," : "", index+1);
      strcat(generated, parameter);
    }
    sql = generated;
  }
  rc = sqlite3_prepare_v2(db, sql, -1, &statement, 0);
  if( rc!=SQLITE_OK ){ put_db_error(sqlite3_extended_errcode(db)); return; }

  /* Bind each tagged value by decoding the object it arrived in. */
  {
    const char *scan = at;
    int index = 0;
    while( *scan && *scan!=']' && index<count ){
      if( *scan=='{' ){
        const char *end = strchr(scan, '}');
        static char object[1 << 21];
        static char klass[16];
        static char hex[1 << 20];
        size_t length;
        if( end==0 ) break;
        length = (size_t)(end - scan + 1);
        if( length >= sizeof(object) ) break;
        memcpy(object, scan, length);
        object[length] = 0;
        index++;
        if( !string_field(object, "class", klass, sizeof(klass)) ){
          sqlite3_finalize(statement);
          put_error(SQLITE_MISUSE, "a value has no class");
          return;
        }
        if( strcmp(klass, "null")==0 ){
          sqlite3_bind_null(statement, index);
        }else if( strcmp(klass, "integer")==0 ){
          int bytes;
          sqlite3_int64 value = 0;
          int byte;
          string_field(object, "be_hex", hex, sizeof(hex));
          bytes = decode_hex(hex, strlen(hex), payload, sizeof(payload));
          if( bytes!=8 ){ sqlite3_finalize(statement); put_error(SQLITE_MISUSE, "integer is not eight bytes"); return; }
          for(byte=0; byte<8; byte++) value = (value<<8) | payload[byte];
          sqlite3_bind_int64(statement, index, value);
        }else if( strcmp(klass, "real")==0 ){
          int bytes;
          sqlite3_uint64 bits = 0;
          double value;
          int byte;
          string_field(object, "ieee754_hex", hex, sizeof(hex));
          bytes = decode_hex(hex, strlen(hex), payload, sizeof(payload));
          if( bytes!=8 ){ sqlite3_finalize(statement); put_error(SQLITE_MISUSE, "real is not eight bytes"); return; }
          for(byte=0; byte<8; byte++) bits = (bits<<8) | payload[byte];
          memcpy(&value, &bits, 8);
          sqlite3_bind_double(statement, index, value);
        }else if( strcmp(klass, "text")==0 ){
          int bytes;
          string_field(object, "utf8_hex", hex, sizeof(hex));
          bytes = decode_hex(hex, strlen(hex), payload, sizeof(payload));
          if( bytes<0 ){ sqlite3_finalize(statement); put_error(SQLITE_MISUSE, "text is not hex"); return; }
          sqlite3_bind_text(statement, index, (const char *)payload, bytes, SQLITE_TRANSIENT);
        }else if( strcmp(klass, "blob")==0 ){
          int bytes;
          string_field(object, "hex", hex, sizeof(hex));
          bytes = decode_hex(hex, strlen(hex), payload, sizeof(payload));
          if( bytes<0 ){ sqlite3_finalize(statement); put_error(SQLITE_MISUSE, "blob is not hex"); return; }
          sqlite3_bind_blob(statement, index, payload, bytes, SQLITE_TRANSIENT);
        }else{
          sqlite3_finalize(statement);
          put_error(SQLITE_MISUSE, "unknown value class");
          return;
        }
        scan = end + 1;
        continue;
      }
      scan++;
    }
  }

  columns = sqlite3_column_count(statement);
  rc = sqlite3_step(statement);
  if( rc!=SQLITE_ROW ){
    int code = sqlite3_extended_errcode(db);
    sqlite3_finalize(statement);
    put_error(code, sqlite3_errmsg(db));
    return;
  }
  fputs("{\"ok\":true,\"rows\":[[", stdout);
  for(column=0; column<columns; column++){
    if( column ) putchar(',');
    put_column(statement, column);
  }
  fputs("]]", stdout);
  sqlite3_finalize(statement);
  put_state();
  fputs("}\n", stdout);
  fflush(stdout);
  (void)owned;
}

/* The run-time limits, by the name sqlite3_limit() documents. */
static const struct { const char *name; int id; } kLimits[] = {
  { "SQLITE_LIMIT_LENGTH",              SQLITE_LIMIT_LENGTH },
  { "SQLITE_LIMIT_SQL_LENGTH",          SQLITE_LIMIT_SQL_LENGTH },
  { "SQLITE_LIMIT_COLUMN",              SQLITE_LIMIT_COLUMN },
  { "SQLITE_LIMIT_EXPR_DEPTH",          SQLITE_LIMIT_EXPR_DEPTH },
  { "SQLITE_LIMIT_COMPOUND_SELECT",     SQLITE_LIMIT_COMPOUND_SELECT },
  { "SQLITE_LIMIT_VDBE_OP",             SQLITE_LIMIT_VDBE_OP },
  { "SQLITE_LIMIT_FUNCTION_ARG",        SQLITE_LIMIT_FUNCTION_ARG },
  { "SQLITE_LIMIT_ATTACHED",            SQLITE_LIMIT_ATTACHED },
  { "SQLITE_LIMIT_LIKE_PATTERN_LENGTH", SQLITE_LIMIT_LIKE_PATTERN_LENGTH },
  { "SQLITE_LIMIT_VARIABLE_NUMBER",     SQLITE_LIMIT_VARIABLE_NUMBER },
  { "SQLITE_LIMIT_TRIGGER_DEPTH",       SQLITE_LIMIT_TRIGGER_DEPTH },
  { "SQLITE_LIMIT_WORKER_THREADS",      SQLITE_LIMIT_WORKER_THREADS },
};

/* Reports every run-time limit at its current (default) value.
**
** A limit that differs from SQLite's is a parity deviation that no query can
** show until the day it refuses something SQLite allowed, so the harness asks
** for the numbers directly rather than inferring them.
*/
static void do_limits(void){
  size_t index;
  int owned = 0;
  if( db==0 ){
    int rc = sqlite3_open(":memory:", &db);
    if( rc!=SQLITE_OK ){ put_db_error(rc); return; }
    owned = 1;
  }
  fputs("{\"ok\":true,\"rows\":[", stdout);
  for(index=0; index<sizeof(kLimits)/sizeof(kLimits[0]); index++){
    int value = sqlite3_limit(db, kLimits[index].id, -1);
    unsigned char be[8];
    int byte;
    sqlite3_int64 wide = value;
    for(byte=7; byte>=0; byte--){ be[byte] = (unsigned char)(wide & 0xff); wide >>= 8; }
    if( index ) putchar(',');
    fputs("[{\"class\":\"text\",\"utf8_hex\":\"", stdout);
    {
      const char *scan = kLimits[index].name;
      while( *scan ){ printf("%02x", (unsigned char)*scan); scan++; }
    }
    fputs("\"},{\"class\":\"integer\",\"be_hex\":\"", stdout);
    for(byte=0; byte<8; byte++) printf("%02x", be[byte]);
    fputs("\"}]", stdout);
  }
  fputs("]", stdout);
  put_state();
  fputs("}\n", stdout);
  fflush(stdout);
  (void)owned;
}

/* Closes the open database. */
static void do_close(void){
  if( db ){ sqlite3_close(db); db = 0; }
  fputs("{\"ok\":true}\n", stdout);
  fflush(stdout);
}

/* Reports what this driver is. */
static void do_hello(void){
  printf("{\"ok\":true,\"driver\":\"sqlite\",\"version\":");
  put_json_string(sqlite3_libversion());
  printf(",\"source_id\":");
  put_json_string(sqlite3_sourceid());
  fputs("}\n", stdout);
  fflush(stdout);
}

/* Reads commands until stdin closes or bye arrives. */
int main(void){
  static char line[MAX_LINE];
  static char op[32];
  static char sql[1 << 20];
  while( fgets(line, sizeof(line), stdin) ){
    if( !string_field(line, "op", op, sizeof(op)) ){
      put_error(SQLITE_MISUSE, "no op");
      continue;
    }
    if( strcmp(op, "bye")==0 ) break;
    if( strcmp(op, "hello")==0 ){ do_hello(); continue; }
    if( strcmp(op, "open")==0 ){ do_open(line); continue; }
    if( strcmp(op, "close")==0 ){ do_close(); continue; }
    if( strcmp(op, "echo")==0 ){ do_bound(line, 0); continue; }
    if( strcmp(op, "limits")==0 ){ do_limits(); continue; }
    if( strcmp(op, "bind")==0 ){
      if( !string_field(line, "sql", sql, sizeof(sql)) ){ put_error(SQLITE_MISUSE, "bind needs sql"); continue; }
      do_bound(line, sql);
      continue;
    }
    if( strcmp(op, "exec")==0 ){
      if( !string_field(line, "sql", sql, sizeof(sql)) ){ put_error(SQLITE_MISUSE, "exec needs sql"); continue; }
      do_exec(sql);
      continue;
    }
    if( strcmp(op, "query")==0 ){
      if( !string_field(line, "sql", sql, sizeof(sql)) ){ put_error(SQLITE_MISUSE, "query needs sql"); continue; }
      do_query(sql);
      continue;
    }
    put_error(SQLITE_MISUSE, "unknown op");
  }
  if( db ) sqlite3_close(db);
  return 0;
}
