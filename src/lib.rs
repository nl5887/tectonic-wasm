//! Tectonic WASM wrapper — LaTeX → PDF in the browser
//! Memory-optimized: uses Rc<Vec<u8>> to avoid cloning file data

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::rc::Rc;
use tectonic_bridge_core::{CoreBridgeLauncher, MinimalDriver};
use tectonic_engine_xetex::TexEngine;
use tectonic_engine_xdvipdfmx::XdvipdfmxEngine;
use tectonic_io_base::{InputOrigin, InputHandle, IoProvider, OpenResult, OutputHandle};
use tectonic_status_base::NoopStatusBackend;

extern "C" {
    /// Ask the JS host to provide a file by name.
    /// Returns 1 if the file was found, 0 if not.
    /// On success, the JS side writes a malloc'd pointer and length to the out params.
    fn js_request_file(
        name_ptr: *const u8,
        name_len: usize,
        data_ptr_out: *mut u32,
        data_len_out: *mut u32,
    ) -> i32;
}

thread_local! {
    static OUTPUTS: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
}

struct CaptureWriter {
    name: String,
    buffer: Vec<u8>,
}

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let name = self.name.clone();
            let data = std::mem::take(&mut self.buffer);
            eprintln!("[wasm-io] output_close: {} ({} bytes)", name, data.len());
            OUTPUTS.with(|o| o.borrow_mut().insert(name, data));
        }
    }
}


/// Try to find a fallback font file for a given font name.
/// XeTeX uses [fontname]:mapping=tex-text; syntax — we strip that and find closest match.
fn find_font_fallback(name: &str, inputs: &HashMap<String, Rc<Vec<u8>>>) -> Option<String> {
    let clean = name
        .trim_start_matches('[')
        .split(']').next().unwrap_or(name)
        .split(':').next().unwrap_or(name)
        .split(';').next().unwrap_or(name)
        .trim();

    if clean.is_empty() { return None; }

    // Try with common font extensions
    for ext in &[".otf", ".ttf", ".pfb"] {
        let candidate = format!("{}{}", clean, ext);
        if inputs.contains_key(&candidate) {
            return Some(candidate);
        }
    }

    // Fuzzy match: lmroman9-regular -> lmroman10-regular
    let lower = clean.to_lowercase();
    if let Some(pos) = lower.find(|c: char| c.is_ascii_digit()) {
        let prefix = &lower[..pos];
        let rest = &lower[pos..];
        let style_start = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let style = &rest[style_start..];
        for size in &["10", "12", "7", "17", "5", "6", "8", "9"] {
            for ext in &[".otf", ".ttf", ".pfb"] {
                let candidate = format!("{}{}{}{}", prefix, size, style, ext);
                if inputs.contains_key(&candidate) {
                    eprintln!("[wasm-io] font fallback: '{}' -> '{}'", name, candidate);
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Memory-optimized IoProvider using Rc<Vec<u8>> — cloning shares data, doesn't copy
struct MemoryIo {
    inputs: HashMap<String, Rc<Vec<u8>>>,
    primary_input: Option<Vec<u8>>,
}

impl MemoryIo {
    fn new() -> Self {
        Self { inputs: HashMap::new(), primary_input: None }
    }
}

impl IoProvider for MemoryIo {
    fn output_open_name(&mut self, name: &str) -> OpenResult<OutputHandle> {
        eprintln!("[wasm-io] output_open: {}", name);
        OpenResult::Ok(OutputHandle::new(name, CaptureWriter { name: name.to_string(), buffer: Vec::new() }))
    }

    fn output_open_stdout(&mut self) -> OpenResult<OutputHandle> {
        eprintln!("[wasm-io] output_open_stdout: providing sink");
        OpenResult::Ok(OutputHandle::new("stdout", CaptureWriter { name: "stdout".to_string(), buffer: Vec::new() }))
    }

    fn input_open_name(
        &mut self,
        name: &str,
        _status: &mut dyn tectonic_status_base::StatusBackend,
    ) -> OpenResult<InputHandle> {
        // Check outputs first (multi-pass: .aux, .log, etc.)
        let from_output = OUTPUTS.with(|o| o.borrow().get(name).cloned());
        if let Some(data) = from_output {
            return OpenResult::Ok(InputHandle::new(name, Cursor::new(data), InputOrigin::Other));
        }
        match self.inputs.get(name) {
            Some(data) => {
                let cloned = (**data).clone();
                if name.ends_with(".tfm") {
                    eprintln!("[wasm-io] input_open: {} -> found ({} bytes, first 8: {:?})", 
                        name, cloned.len(), &cloned[..std::cmp::min(8, cloned.len())]);
                } else {
                    eprintln!("[wasm-io] input_open: {} -> found ({} bytes)", name, cloned.len());
                }
                OpenResult::Ok(InputHandle::new(name, Cursor::new(cloned), InputOrigin::Other))
            }
            None => {
                // Try font fallback first
                if let Some(fallback) = find_font_fallback(name, &self.inputs) {
                    if let Some(data) = self.inputs.get(&fallback) {
                        return OpenResult::Ok(InputHandle::new(
                            &fallback, Cursor::new((**data).clone()), InputOrigin::Other,
                        ));
                    }
                }

                // Ask JS host for the file on-demand
                let mut data_ptr: u32 = 0;
                let mut data_len: u32 = 0;
                let name_bytes = name.as_bytes();
                let found = unsafe {
                    js_request_file(
                        name_bytes.as_ptr(),
                        name_bytes.len(),
                        &mut data_ptr as *mut u32,
                        &mut data_len as *mut u32,
                    )
                };

                if found != 0 && data_ptr != 0 && data_len > 0 {
                    let data = unsafe {
                        std::slice::from_raw_parts(data_ptr as *const u8, data_len as usize)
                    }.to_vec();
                    eprintln!("[wasm-io] input_open: {} -> fetched from JS ({} bytes)", name, data.len());
                    // Cache for subsequent requests within this compilation
                    let rc_data = Rc::new(data.clone());
                    self.inputs.insert(name.to_string(), rc_data);
                    OpenResult::Ok(InputHandle::new(name, Cursor::new(data), InputOrigin::Other))
                } else {
                    eprintln!("[wasm-io] input_open: {} -> NOT FOUND", name);
                    OpenResult::NotAvailable
                }
            }
        }
    }

    fn input_open_primary(
        &mut self,
        _status: &mut dyn tectonic_status_base::StatusBackend,
    ) -> OpenResult<InputHandle> {
        eprintln!("[wasm-io] input_open_primary called, has_input={}", self.primary_input.is_some());
        match &self.primary_input {
            Some(data) => {
                eprintln!("[wasm-io] input_open_primary: texput.tex ({} bytes)", data.len());
                OpenResult::Ok(InputHandle::new("texput.tex", Cursor::new(data.clone()), InputOrigin::Other))
            }
            None => OpenResult::NotAvailable,
        }
    }

    fn input_open_format(
        &mut self,
        name: &str,
        status: &mut dyn tectonic_status_base::StatusBackend,
    ) -> OpenResult<InputHandle> {
        let candidates = [
            name.to_string(),
            format!("{}.fmt", name),
            format!("{}-33.fmt", name),
            "latex.fmt".to_string(),
        ];
        for candidate in &candidates {
            if let Some(data) = self.inputs.get(candidate.as_str()) {
                eprintln!("[wasm-io] input_open_format: {} -> found as '{}' ({} bytes)", name, candidate, data.len());
                return OpenResult::Ok(InputHandle::new(candidate, Cursor::new((**data).clone()), InputOrigin::Other));
            }
        }

        // Try fetching the format file from JS before falling back to input_open_name
        for candidate in &candidates {
            let name_bytes = candidate.as_bytes();
            let mut data_ptr: u32 = 0;
            let mut data_len: u32 = 0;
            let found = unsafe {
                js_request_file(
                    name_bytes.as_ptr(),
                    name_bytes.len(),
                    &mut data_ptr as *mut u32,
                    &mut data_len as *mut u32,
                )
            };
            if found != 0 && data_ptr != 0 && data_len > 0 {
                let data = unsafe {
                    std::slice::from_raw_parts(data_ptr as *const u8, data_len as usize)
                }.to_vec();
                eprintln!("[wasm-io] input_open_format: {} -> fetched from JS as '{}' ({} bytes)", name, candidate, data.len());
                let rc_data = Rc::new(data.clone());
                self.inputs.insert(candidate.to_string(), rc_data);
                return OpenResult::Ok(InputHandle::new(candidate, Cursor::new(data), InputOrigin::Other));
            }
        }

        self.input_open_name(name, status)
    }
}

// --- Global state using Rc for zero-copy cloning ---
static mut PRIMARY: Option<Vec<u8>> = None;
static mut PRIMARY_BACKUP: Option<Vec<u8>> = None;
static mut FILES: Option<HashMap<String, Rc<Vec<u8>>>> = None;

fn get_files() -> &'static mut HashMap<String, Rc<Vec<u8>>> {
    unsafe {
        if FILES.is_none() { FILES = Some(HashMap::new()); }
        FILES.as_mut().unwrap()
    }
}

#[no_mangle]
pub extern "C" fn tectonic_wasm_version() -> i32 { 8 }

#[no_mangle]
pub extern "C" fn tectonic_set_input(ptr: *const u8, len: usize) {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    eprintln!("[wasm] set_input: {} bytes", data.len());
    unsafe {
        PRIMARY_BACKUP = Some(data.clone());
        PRIMARY = Some(data);
    }
}

#[no_mangle]
pub extern "C" fn tectonic_add_file(name_ptr: *const u8, name_len: usize, data_ptr: *const u8, data_len: usize) {
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) }.to_string();
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) }.to_vec();
    get_files().insert(name, Rc::new(data));
}

#[no_mangle]
pub extern "C" fn tectonic_compile() -> i32 {
    eprintln!("[wasm] compile starting...");
    OUTPUTS.with(|o| o.borrow_mut().clear());

    let mut io = MemoryIo::new();
    unsafe {
        if let Some(p) = PRIMARY.take() {
            eprintln!("[wasm] primary input: {} bytes", p.len());
            io.primary_input = Some(p);
        }
        if let Some(f) = &FILES {
            eprintln!("[wasm] {} files available (Rc clone = zero-copy)", f.len());
            // Rc clone: only copies the HashMap structure + Rc pointers, NOT file data
            io.inputs = f.iter().map(|(k, v)| (k.clone(), Rc::clone(v))).collect();
        }
    }

    let mut driver = MinimalDriver::new(io);
    let mut status = NoopStatusBackend::default();
    let mut launcher = CoreBridgeLauncher::new(&mut driver, &mut status);
    let mut engine = TexEngine::default();
    engine.halt_on_error_mode(false);

    eprintln!("[wasm] running XeTeX engine (pass 1)...");
    match engine.process(&mut launcher, "latex", "texput") {
        Ok(outcome) => eprintln!("[wasm] XeTeX pass 1 finished: {:?}", outcome),
        Err(e) => {
            eprintln!("[wasm] XeTeX error: {:?}", e);
            return 1;
        }
    }

    // Drop to flush output handles (texput.aux, texput.toc, texput.xdv, etc.)
    drop(launcher);
    drop(driver);
    drop(engine);
    drop(status);

    // Check if we need a second pass (toc, aux files changed)
    let needs_rerun = OUTPUTS.with(|o| {
        let outputs = o.borrow();
        outputs.contains_key("texput.toc") || outputs.contains_key("texput.aux")
    });

    if needs_rerun {
        eprintln!("[wasm] Rerunning XeTeX (pass 2) for TOC/references...");
        
        // Re-set primary input
        unsafe {
            if let Some(p) = &PRIMARY_BACKUP {
                PRIMARY = Some(p.clone());
            }
        }

        let mut io_pass2 = MemoryIo::new();
        unsafe {
            if let Some(p) = PRIMARY.take() { io_pass2.primary_input = Some(p); }
            if let Some(f) = &FILES {
                io_pass2.inputs = f.iter().map(|(k, v)| (k.clone(), Rc::clone(v))).collect();
            }
        }

        let mut driver2 = MinimalDriver::new(io_pass2);
        let mut status2 = NoopStatusBackend::default();
        let mut launcher2 = CoreBridgeLauncher::new(&mut driver2, &mut status2);
        let mut engine2 = TexEngine::default();
        engine2.halt_on_error_mode(false);

        match engine2.process(&mut launcher2, "latex", "texput") {
            Ok(outcome) => eprintln!("[wasm] XeTeX pass 2 finished: {:?}", outcome),
            Err(e) => eprintln!("[wasm] XeTeX pass 2 error: {:?}", e),
        }

        drop(launcher2);
        drop(driver2);
        drop(engine2);
        drop(status2);
    }

    // Log outputs
    OUTPUTS.with(|o| {
        let outputs = o.borrow();
        for (name, data) in outputs.iter() {
            eprintln!("[wasm] output available: {} ({} bytes)", name, data.len());
            if name.ends_with(".log") || name == "stdout" {
                if let Ok(text) = std::str::from_utf8(data) {
                    eprintln!("[wasm] --- {} contents ---\n{}\n[wasm] --- end {} ---", name, text, name);
                }
            }
        }
    });

    // Step 2: XDV → PDF
    let mut io2 = MemoryIo::new();
    unsafe {
        if let Some(f) = &FILES {
            io2.inputs = f.iter().map(|(k, v)| (k.clone(), Rc::clone(v))).collect();
        }
    }

    let mut driver2 = MinimalDriver::new(io2);
    let mut status2 = NoopStatusBackend::default();
    let mut launcher2 = CoreBridgeLauncher::new(&mut driver2, &mut status2);
    let mut pdf_engine = XdvipdfmxEngine::default();

    eprintln!("[wasm] running xdvipdfmx...");
    match pdf_engine.process(&mut launcher2, "texput.xdv", "texput.pdf") {
        Ok(()) => eprintln!("[wasm] xdvipdfmx finished successfully"),
        Err(e) => eprintln!("[wasm] xdvipdfmx error: {:?}", e),
    }

    drop(launcher2);
    drop(driver2);
    
    0
}

#[no_mangle]
pub extern "C" fn tectonic_get_output_size(name_ptr: *const u8, name_len: usize) -> usize {
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };
    OUTPUTS.with(|o| o.borrow().get(name).map_or(0, |v| v.len()))
}

#[no_mangle]
pub extern "C" fn tectonic_get_output(name_ptr: *const u8, name_len: usize, buf_ptr: *mut u8, buf_len: usize) -> usize {
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };
    OUTPUTS.with(|o| {
        if let Some(data) = o.borrow().get(name) {
            let len = data.len().min(buf_len);
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr, len); }
            len
        } else { 0 }
    })
}

#[no_mangle]
pub extern "C" fn tectonic_list_outputs(buf_ptr: *mut u8, buf_len: usize) -> usize {
    OUTPUTS.with(|o| {
        let list: String = o.borrow().keys().map(|k| k.as_str()).collect::<Vec<_>>().join("\n");
        let bytes = list.as_bytes();
        let len = bytes.len().min(buf_len);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr, len); }
        len
    })
}

#[no_mangle]
pub extern "C" fn tectonic_create_format() -> i32 {
    eprintln!("[wasm] creating format file (INITEX mode)...");
    OUTPUTS.with(|o| o.borrow_mut().clear());

    let mut io = MemoryIo::new();
    // Set latex.ltx as primary input for format generation
    unsafe {
        if let Some(f) = &FILES {
            eprintln!("[wasm] {} files available for format creation", f.len());
            io.inputs = f.iter().map(|(k, v)| (k.clone(), Rc::clone(v))).collect();
            // Use xelatex.ini as primary input (which loads latex.ltx)
            if let Some(ltx) = f.get("xelatex.ini") {
                io.primary_input = Some((**ltx).clone());
                eprintln!("[wasm] Set xelatex.ini ({} bytes) as primary input", ltx.len());
            } else if let Some(ltx) = f.get("latex.ltx") {
                io.primary_input = Some((**ltx).clone());
                eprintln!("[wasm] Set latex.ltx ({} bytes) as primary input (fallback)", ltx.len());
            } else {
                eprintln!("[wasm] ERROR: latex.ltx not found!");
                return 1;
            }
        }
    }

    let mut driver = MinimalDriver::new(io);
    let mut status = NoopStatusBackend::default();
    let mut launcher = CoreBridgeLauncher::new(&mut driver, &mut status);
    let mut engine = TexEngine::default();
    engine.initex_mode(true);
    engine.halt_on_error_mode(false);

    eprintln!("[wasm] running XeTeX in INITEX mode...");
    match engine.process(&mut launcher, "latex", "xelatex.ini") {
        Ok(outcome) => eprintln!("[wasm] INITEX finished: {:?}", outcome),
        Err(e) => {
            eprintln!("[wasm] INITEX error: {:?}", e);
            return 1;
        }
    }

    drop(launcher);
    drop(driver);

    // The format should be in OUTPUTS as "latex.fmt"
    OUTPUTS.with(|o| {
        let outputs = o.borrow();
        if let Some(fmt) = outputs.get("latex.fmt") {
            eprintln!("[wasm] Format file generated: {} bytes", fmt.len());
            // Store it in FILES for use by tectonic_compile
            get_files().insert("latex.fmt".to_string(), Rc::new(fmt.clone()));
        } else {
            eprintln!("[wasm] WARNING: No format file produced!");
            for (name, data) in outputs.iter() {
                eprintln!("[wasm] Output: {} ({} bytes)", name, data.len());
            }
        }
    });

    0
}
