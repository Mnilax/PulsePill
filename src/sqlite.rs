//! Minimal read-only SQLite binding over Windows' built-in `winsqlite3.dll`.
//!
//! Windows 10/11 ships SQLite (with JSON1) in System32, so the app needs neither a C compiler
//! nor a bundled copy of SQLite. Functions are resolved once with `GetProcAddress`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READONLY: c_int = 0x1;
const SQLITE_OPEN_NOMUTEX: c_int = 0x8000;
const SQLITE_TRANSIENT: isize = -1;
pub const SQLITE_BUSY: c_int = 5;
pub const SQLITE_LOCKED: c_int = 6;

type Db = c_void;
type Stmt = c_void;

struct Api {
    open_v2: unsafe extern "system" fn(*const c_char, *mut *mut Db, c_int, *const c_char) -> c_int,
    close_v2: unsafe extern "system" fn(*mut Db) -> c_int,
    errmsg: unsafe extern "system" fn(*mut Db) -> *const c_char,
    busy_timeout: unsafe extern "system" fn(*mut Db, c_int) -> c_int,
    exec: unsafe extern "system" fn(*mut Db, *const c_char, *const c_void, *mut c_void, *mut *mut c_char) -> c_int,
    prepare_v2: unsafe extern "system" fn(*mut Db, *const c_char, c_int, *mut *mut Stmt, *mut *const c_char) -> c_int,
    bind_text: unsafe extern "system" fn(*mut Stmt, c_int, *const c_char, c_int, isize) -> c_int,
    step: unsafe extern "system" fn(*mut Stmt) -> c_int,
    column_text: unsafe extern "system" fn(*mut Stmt, c_int) -> *const u8,
    column_bytes: unsafe extern "system" fn(*mut Stmt, c_int) -> c_int,
    column_int64: unsafe extern "system" fn(*mut Stmt, c_int) -> i64,
    finalize: unsafe extern "system" fn(*mut Stmt) -> c_int,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let name: Vec<u16> = "winsqlite3.dll\0".encode_utf16().collect();
        let lib = LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if lib.is_null() {
            return None;
        }
        macro_rules! sym {
            ($n:literal) => {
                std::mem::transmute(GetProcAddress(lib, concat!($n, "\0").as_ptr())?)
            };
        }
        Some(Api {
            open_v2: sym!("sqlite3_open_v2"),
            close_v2: sym!("sqlite3_close_v2"),
            errmsg: sym!("sqlite3_errmsg"),
            busy_timeout: sym!("sqlite3_busy_timeout"),
            exec: sym!("sqlite3_exec"),
            prepare_v2: sym!("sqlite3_prepare_v2"),
            bind_text: sym!("sqlite3_bind_text"),
            step: sym!("sqlite3_step"),
            column_text: sym!("sqlite3_column_text"),
            column_bytes: sym!("sqlite3_column_bytes"),
            column_int64: sym!("sqlite3_column_int64"),
            finalize: sym!("sqlite3_finalize"),
        })
    })
    .as_ref()
}

#[derive(Debug, Clone)]
pub struct SqlError {
    pub code: i32,
    pub message: String,
}

impl SqlError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        SqlError { code, message: message.into() }
    }
    pub fn is_transient(&self) -> bool {
        matches!(self.code & 0xFF, SQLITE_BUSY | SQLITE_LOCKED)
    }
}

pub type SqlResult<T> = Result<T, SqlError>;

pub struct Connection {
    db: *mut Db,
}

// The connection is only ever used from the monitor thread.
unsafe impl Send for Connection {}

impl Connection {
    pub fn open_read_only(path: &str) -> SqlResult<Connection> {
        let api = api().ok_or_else(|| SqlError::new(-1, "winsqlite3.dll is not available"))?;
        let c_path = CString::new(path).map_err(|_| SqlError::new(-1, "bad path"))?;
        let mut db = std::ptr::null_mut();
        let rc = unsafe { (api.open_v2)(c_path.as_ptr(), &mut db, SQLITE_OPEN_READONLY | SQLITE_OPEN_NOMUTEX, std::ptr::null()) };
        let conn = Connection { db };
        if rc != SQLITE_OK {
            return Err(conn.error(rc));
        }
        unsafe { (api.busy_timeout)(db, 250) };
        // Keep the page cache tiny: the whole point of this app is a small footprint.
        conn.exec("PRAGMA query_only=1; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=MEMORY;")?;
        Ok(conn)
    }

    fn error(&self, rc: c_int) -> SqlError {
        let msg = match api() {
            Some(api) if !self.db.is_null() => unsafe { CStr::from_ptr((api.errmsg)(self.db)).to_string_lossy().into_owned() },
            _ => format!("sqlite error {rc}"),
        };
        SqlError::new(rc, msg)
    }

    pub fn exec(&self, sql: &str) -> SqlResult<()> {
        let api = api().unwrap();
        let c = CString::new(sql).unwrap();
        let rc = unsafe { (api.exec)(self.db, c.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if rc == SQLITE_OK { Ok(()) } else { Err(self.error(rc)) }
    }

    /// Runs `sql` with text parameters, calling `each` for every row.
    pub fn query(&self, sql: &str, params: &[&str], mut each: impl FnMut(&Row)) -> SqlResult<()> {
        let api = api().unwrap();
        let c = CString::new(sql).unwrap();
        let mut stmt = std::ptr::null_mut();
        let rc = unsafe { (api.prepare_v2)(self.db, c.as_ptr(), -1, &mut stmt, std::ptr::null_mut()) };
        if rc != SQLITE_OK {
            return Err(self.error(rc));
        }
        let guard = StmtGuard(stmt);
        for (i, p) in params.iter().enumerate() {
            let rc = unsafe { (api.bind_text)(stmt, i as c_int + 1, p.as_ptr() as *const c_char, p.len() as c_int, SQLITE_TRANSIENT) };
            if rc != SQLITE_OK {
                return Err(self.error(rc));
            }
        }
        loop {
            match unsafe { (api.step)(guard.0) } {
                SQLITE_ROW => each(&Row { stmt: guard.0 }),
                SQLITE_DONE => return Ok(()),
                rc => return Err(self.error(rc)),
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let (Some(api), false) = (api(), self.db.is_null()) {
            unsafe { (api.close_v2)(self.db) };
        }
    }
}

struct StmtGuard(*mut Stmt);
impl Drop for StmtGuard {
    fn drop(&mut self) {
        unsafe { (api().unwrap().finalize)(self.0) };
    }
}

pub struct Row {
    stmt: *mut Stmt,
}

impl Row {
    pub fn text(&self, i: i32) -> Option<String> {
        let api = api().unwrap();
        unsafe {
            let p = (api.column_text)(self.stmt, i);
            if p.is_null() {
                return None;
            }
            let n = (api.column_bytes)(self.stmt, i) as usize;
            Some(String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned())
        }
    }
    pub fn int(&self, i: i32) -> i64 {
        unsafe { (api().unwrap().column_int64)(self.stmt, i) }
    }
    pub fn bool(&self, i: i32) -> bool {
        self.int(i) != 0
    }
}
