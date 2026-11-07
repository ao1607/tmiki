/*
** tmi: SQLite 3.50.4 (the official amalgamation in sqlite3.c, unmodified),
** the SQLite CPython 3.13 carries, plus sqlite3_set_errmsg(): SQLite 3.51
** added it and rusqlite 0.40 calls it when a Rust callback (a user function,
** a virtual table) fails. tmi registers none, so this only has to link and
** behave sensibly: it sets the connection's error code and message, as the
** 3.51 function does.
*/
#include "sqlite3.c"

SQLITE_API int sqlite3_set_errmsg(sqlite3 *db, int errcode, const char *zErrMsg){
#ifdef SQLITE_ENABLE_API_ARMOR
  if( !sqlite3SafetyCheckOk(db) ) return SQLITE_MISUSE_BKPT;
#endif
  sqlite3_mutex_enter(db->mutex);
  if( zErrMsg ){
    sqlite3ErrorWithMsg(db, errcode, "%s", zErrMsg);
  }else{
    sqlite3Error(db, errcode);
  }
  sqlite3_mutex_leave(db->mutex);
  return SQLITE_OK;
}
