//! A SQLite file layer for tests that models macOS background I/O throttling: every write and sync made from a
//! thread at background QoS waits a fixed time first, as the compactor's I/O did on a busy disk (24 Oct 02:30, 31 Oct
//! 23:51: one step held the archive's write lock for about a minute); a read waits a tenth of it (most reads come from
//! the file cache, which throttling does not slow). Threads at any other QoS run at full speed. The wait is per call,
//! so a transaction's cost grows with the pages it touches: a long step becomes long, a short one stays short, the
//! same way on every run.
//!
//! `install` makes it SQLite's default for every connection the test process opens afterwards (the store's own
//! connections included). Nothing here touches the archive's code.
#![allow(dead_code, non_snake_case)]
use rusqlite::ffi;
use std::{
    ffi::{c_int, c_void},
    sync::{
        atomic::{AtomicU64, Ordering},
        Once,
    },
    time::Duration,
};

/// Microseconds a background-QoS write waits; a read a tenth of it, a sync ten times as long.
static IO_US: AtomicU64 = AtomicU64::new(2_000);
/// Throttled calls made so far (reads, writes, syncs).
pub static THROTTLED: AtomicU64 = AtomicU64::new(0);

static mut REAL: *mut ffi::sqlite3_vfs = std::ptr::null_mut();
static mut VFS: std::mem::MaybeUninit<ffi::sqlite3_vfs> = std::mem::MaybeUninit::uninit();
static INSTALL: Once = Once::new();

#[repr(C)]
struct Shim {
    base: ffi::sqlite3_file,
    real: *mut ffi::sqlite3_file,
}

extern "C" {
    fn qos_class_self() -> libc::qos_class_t;
}

fn throttled() -> bool {
    unsafe { qos_class_self() as u32 == libc::qos_class_t::QOS_CLASS_BACKGROUND as u32 }
}
/// Waits `tenths` tenths of the per-write wait.
fn pause(tenths: u64) {
    if throttled() {
        THROTTLED.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(Duration::from_micros(IO_US.load(Ordering::Relaxed) * tenths / 10));
    }
}

/// Sets the per-call wait for background-QoS I/O.
pub fn set_io(wait: Duration) {
    IO_US.store(wait.as_micros() as u64, Ordering::Relaxed);
}

/// Registers the layer as SQLite's default VFS (once per process).
pub fn install() {
    INSTALL.call_once(|| unsafe {
        let real = ffi::sqlite3_vfs_find(std::ptr::null());
        assert!(!real.is_null());
        REAL = real;
        let mut vfs = std::ptr::read(real);
        vfs.szOsFile = (std::mem::size_of::<Shim>() + (*real).szOsFile as usize) as c_int;
        vfs.zName = c"throttle".as_ptr();
        vfs.pNext = std::ptr::null_mut();
        vfs.xOpen = Some(open);
        let slot = std::ptr::addr_of_mut!(VFS);
        (*slot).write(vfs);
        assert_eq!(ffi::sqlite3_vfs_register((*slot).as_mut_ptr(), 1), ffi::SQLITE_OK);
    });
}

unsafe fn real(file: *mut ffi::sqlite3_file) -> *mut ffi::sqlite3_file {
    unsafe { (*(file as *mut Shim)).real }
}
unsafe fn methods(file: *mut ffi::sqlite3_file) -> &'static ffi::sqlite3_io_methods {
    unsafe { &*(*real(file)).pMethods }
}

unsafe extern "C" fn open(_vfs: *mut ffi::sqlite3_vfs, name: ffi::sqlite3_filename, file: *mut ffi::sqlite3_file, flags: c_int, out: *mut c_int) -> c_int {
    unsafe {
        let shim = file as *mut Shim;
        let inner = (file as *mut u8).add(std::mem::size_of::<Shim>()) as *mut ffi::sqlite3_file;
        (*shim).real = inner;
        (*shim).base.pMethods = std::ptr::null();
        let rc = ((*REAL).xOpen.unwrap())(REAL, name, inner, flags, out);
        if rc == ffi::SQLITE_OK && !(*inner).pMethods.is_null() {
            (*shim).base.pMethods = &METHODS;
        }
        rc
    }
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(file_size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(check_reserved),
    xFileControl: Some(file_control),
    xSectorSize: Some(sector_size),
    xDeviceCharacteristics: Some(device),
    xShmMap: Some(shm_map),
    xShmLock: Some(shm_lock),
    xShmBarrier: Some(shm_barrier),
    xShmUnmap: Some(shm_unmap),
    xFetch: Some(fetch),
    xUnfetch: Some(unfetch),
};

unsafe extern "C" fn close(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let rc = (methods(f).xClose.unwrap())(real(f));
        (*f).pMethods = std::ptr::null();
        rc
    }
}
unsafe extern "C" fn read(f: *mut ffi::sqlite3_file, buf: *mut c_void, n: c_int, off: ffi::sqlite3_int64) -> c_int {
    pause(1);
    unsafe { (methods(f).xRead.unwrap())(real(f), buf, n, off) }
}
unsafe extern "C" fn write(f: *mut ffi::sqlite3_file, buf: *const c_void, n: c_int, off: ffi::sqlite3_int64) -> c_int {
    pause(10);
    unsafe { (methods(f).xWrite.unwrap())(real(f), buf, n, off) }
}
unsafe extern "C" fn truncate(f: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    unsafe { (methods(f).xTruncate.unwrap())(real(f), size) }
}
unsafe extern "C" fn sync(f: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    pause(100);
    unsafe { (methods(f).xSync.unwrap())(real(f), flags) }
}
unsafe extern "C" fn file_size(f: *mut ffi::sqlite3_file, size: *mut ffi::sqlite3_int64) -> c_int {
    unsafe { (methods(f).xFileSize.unwrap())(real(f), size) }
}
unsafe extern "C" fn lock(f: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe { (methods(f).xLock.unwrap())(real(f), level) }
}
unsafe extern "C" fn unlock(f: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe { (methods(f).xUnlock.unwrap())(real(f), level) }
}
unsafe extern "C" fn check_reserved(f: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    unsafe { (methods(f).xCheckReservedLock.unwrap())(real(f), out) }
}
unsafe extern "C" fn file_control(f: *mut ffi::sqlite3_file, op: c_int, arg: *mut c_void) -> c_int {
    unsafe { (methods(f).xFileControl.unwrap())(real(f), op, arg) }
}
unsafe extern "C" fn sector_size(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe { (methods(f).xSectorSize.unwrap())(real(f)) }
}
unsafe extern "C" fn device(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe { (methods(f).xDeviceCharacteristics.unwrap())(real(f)) }
}
unsafe extern "C" fn shm_map(f: *mut ffi::sqlite3_file, page: c_int, size: c_int, extend: c_int, out: *mut *mut c_void) -> c_int {
    unsafe {
        match methods(f).xShmMap {
            Some(m) => m(real(f), page, size, extend, out),
            None => ffi::SQLITE_IOERR,
        }
    }
}
unsafe extern "C" fn shm_lock(f: *mut ffi::sqlite3_file, offset: c_int, n: c_int, flags: c_int) -> c_int {
    unsafe {
        match methods(f).xShmLock {
            Some(m) => m(real(f), offset, n, flags),
            None => ffi::SQLITE_IOERR,
        }
    }
}
unsafe extern "C" fn shm_barrier(f: *mut ffi::sqlite3_file) {
    unsafe {
        if let Some(m) = methods(f).xShmBarrier {
            m(real(f))
        }
    }
}
unsafe extern "C" fn shm_unmap(f: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    unsafe {
        match methods(f).xShmUnmap {
            Some(m) => m(real(f), delete),
            None => ffi::SQLITE_OK,
        }
    }
}
unsafe extern "C" fn fetch(_f: *mut ffi::sqlite3_file, _off: ffi::sqlite3_int64, _n: c_int, out: *mut *mut c_void) -> c_int {
    unsafe {
        // No memory-mapped reads: every page goes through `read`, so throttling sees it.
        *out = std::ptr::null_mut();
        ffi::SQLITE_OK
    }
}
unsafe extern "C" fn unfetch(_f: *mut ffi::sqlite3_file, _off: ffi::sqlite3_int64, _p: *mut c_void) -> c_int {
    ffi::SQLITE_OK
}
