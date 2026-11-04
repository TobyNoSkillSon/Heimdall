//! Resident offline prototype driver. The companion script supplies archive
//! turns and controls replay; this binary never opens an archive or an app.
use anyhow::{bail, ensure, Context, Result};
use libloading::Library;
use serde_json::{json, Value};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    io::{self, BufRead, Write},
    time::Instant,
};

type Free = unsafe extern "C" fn(*mut c_char);
unsafe fn take(ptr: *mut c_char, free: Free) -> Result<Value> {
    ensure!(!ptr.is_null(), "native null response");
    let text = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    free(ptr);
    Ok(serde_json::from_str(&text)?)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 5, "expected LIBRARY MODEL PROJECTOR WINDOW");
    let started = Instant::now();
    unsafe {
        let lib = Library::new(&args[1])?;
        let create = lib.get::<unsafe extern "C" fn(
            *const c_char,
            *const c_char,
            i32,
            *mut *mut c_char,
        ) -> *mut c_void>(b"heimdall_context_create\0")?;
        let free = *lib.get::<Free>(b"heimdall_d1_free\0")?;
        let destroy = lib.get::<unsafe extern "C" fn(*mut c_void)>(b"heimdall_d1_destroy\0")?;
        let info = lib
            .get::<unsafe extern "C" fn(*mut c_void) -> *mut c_char>(b"heimdall_context_info\0")?;
        let tokens = lib.get::<unsafe extern "C" fn(*mut c_void, *const c_char) -> i32>(
            b"heimdall_context_tokens\0",
        )?;
        let decide = lib.get::<unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const f32,
            usize,
            *mut *mut c_char,
        ) -> *mut c_char>(b"heimdall_context_decide\0")?;
        let mut error = std::ptr::null_mut();
        let handle = create(
            CString::new(args[2].as_str())?.as_ptr(),
            CString::new(args[3].as_str())?.as_ptr(),
            args[4].parse()?,
            &mut error,
        );
        if !error.is_null() {
            let message = CStr::from_ptr(error).to_string_lossy().into_owned();
            free(error);
            bail!(message);
        }
        ensure!(!handle.is_null(), "null prototype handle");
        let run = (|| -> Result<()> {
            println!(
                "{}",
                json!({"ready":take(info(handle),free)?,"load_ms":started.elapsed().as_secs_f64()*1000.})
            );
            io::stdout().flush()?;
            for line in io::stdin().lock().lines() {
                ensure!(
                    started.elapsed().as_secs() < 240,
                    "four-minute process budget exhausted"
                );
                let req: Value = serde_json::from_str(&line?)?;
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let result = match req["op"].as_str().unwrap_or("decide") {
                    "tokens" => {
                        json!({"tokens":tokens(handle,CString::new(req["text"].as_str().context("text")?)?.as_ptr())})
                    }
                    "decide" => {
                        let pcm = if let Some(path) = req["pcm_path"].as_str() {
                            let bytes = std::fs::read(path)?;
                            ensure!(
                                bytes.len() % 4 == 0 && bytes.len() <= 480000 * 4,
                                "PCM size"
                            );
                            bytes
                                .chunks_exact(4)
                                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                                .collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        };
                        let body = CString::new(serde_json::to_string(&req["request"])?)?;
                        let timer = Instant::now();
                        let out = decide(
                            handle,
                            body.as_ptr(),
                            if pcm.is_empty() {
                                std::ptr::null()
                            } else {
                                pcm.as_ptr()
                            },
                            pcm.len(),
                            &mut error,
                        );
                        let ms = timer.elapsed().as_secs_f64() * 1000.;
                        if !error.is_null() {
                            let message = CStr::from_ptr(error).to_string_lossy().into_owned();
                            free(error);
                            error = std::ptr::null_mut();
                            json!({"error":message,"ms":ms})
                        } else {
                            json!({"output":take(out,free)?,"ms":ms})
                        }
                    }
                    other => bail!("unsupported operation {other}"),
                };
                println!("{}", json!({"id":id,"result":result}));
                io::stdout().flush()?;
            }
            Ok(())
        })();
        destroy(handle);
        run
    }
}
