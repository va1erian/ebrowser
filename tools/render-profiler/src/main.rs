//! A poor-man's sampling profiler for one litehtml render.
//!
//! It renders an HTML file the way `egui-litehtml-webview`'s worker does
//! (parse -> layout -> paint into a reused `PixbufContainer`), repeated a few
//! times, while a second thread suspends the render thread about every 0.7 ms,
//! walks its stack with dbghelp, and resumes it. Each sample is tagged with the
//! phase the render thread was in. At the end it prints, per phase, the hottest
//! functions by *self* time (the top frame) and by *inclusive* time (anywhere
//! in the stack). C++ (litehtml) and Rust frames both resolve, because dbghelp
//! reads the PDB the build produced.
//!
//! Why not a real profiler: the machine this was written on had no admin rights
//! (ETW-based tools such as samply or WPR need them) and no debugger installed.
//! This needs neither. See docs/PERFORMANCE.md for how to use it and how to read
//! the output.
//!
//! Windows, MSVC, x86_64 only. Usage:
//!
//! ```text
//! render-profiler <html-file> [width-pt=1000] [iterations=3] [scale=1.25]
//! ```

use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CStr};
use std::mem::{size_of, zeroed};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::time::{Duration, Instant};

use litehtml::email::EMAIL_MASTER_CSS;
use litehtml::pixbuf::PixbufContainer;
use litehtml::{Document, DrawContext};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleA;
use windows_sys::Win32::System::Threading::*;

/// Which part of the render the worker is in, updated by the worker and read by
/// the sampler. `idle` is the tail of an iteration: dropping the `Document`.
const PHASES: [&str; 5] = ["init(fonts)", "parse", "layout", "paint", "idle"];
static PHASE: AtomicU32 = AtomicU32::new(0);
static TID: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// Same steps as `Worker::layout_and_draw` in `egui-litehtml-webview`, with the
/// container reused across iterations (so font and glyph caches warm up, as they
/// do across messages in the app). Iteration 0 is the cold, first-message case.
fn worker(html: String, width: f32, scale: f32, iterations: usize) {
    unsafe { TID.store(GetCurrentThreadId(), SeqCst) };
    PHASE.store(0, SeqCst);
    let t = Instant::now();
    // Loads the system fonts: paid once per worker thread in the app.
    let mut container = PixbufContainer::new_with_scale(1, 4000, scale);
    println!("init container (font system): {:?}", t.elapsed());

    for i in 0..iterations {
        container.resize_with_scale(width.ceil() as u32, 4000, scale);

        PHASE.store(1, SeqCst);
        let t = Instant::now();
        let mut doc = Document::from_html(&html, &mut container, None, Some(EMAIL_MASTER_CSS)).unwrap();
        let parse = t.elapsed();

        PHASE.store(2, SeqCst);
        let t = Instant::now();
        let _ = doc.render(width);
        let layout = t.elapsed();

        PHASE.store(3, SeqCst);
        let t = Instant::now();
        let height = doc.height();
        doc.draw(DrawContext::default(), 0.0, 0.0, None);
        let paint = t.elapsed();

        PHASE.store(4, SeqCst);
        drop(doc);
        println!(
            "iter {i}: parse {parse:?}  layout {layout:?}  paint {paint:?}  total {:?}  (content height {height:.0})",
            parse + layout + paint
        );
    }
    DONE.store(true, SeqCst);
}

struct Sample {
    phase: u32,
    stack: Vec<u64>,
}

/// Suspend `hthread`, capture up to `buf.len()` return addresses, resume it.
///
/// Nothing in here allocates while the target is suspended: if the target held
/// the process heap lock when it was suspended, an allocation here would
/// deadlock. (dbghelp itself may allocate the first time it sees a module; that
/// happens in the first few samples, and a hang there means "run it again".)
unsafe fn sample_once(hproc: HANDLE, hthread: HANDLE, buf: &mut [u64; 96]) -> usize {
    if SuspendThread(hthread) == u32::MAX {
        return 0;
    }
    let mut n = 0;
    let mut ctx: CONTEXT = zeroed();
    ctx.ContextFlags = 0x10000B; // CONTEXT_FULL for amd64
    if GetThreadContext(hthread, &mut ctx) != 0 {
        let mut frame: STACKFRAME64 = zeroed();
        frame.AddrPC.Offset = ctx.Rip;
        frame.AddrPC.Mode = AddrModeFlat;
        frame.AddrFrame.Offset = ctx.Rbp;
        frame.AddrFrame.Mode = AddrModeFlat;
        frame.AddrStack.Offset = ctx.Rsp;
        frame.AddrStack.Mode = AddrModeFlat;
        while n < buf.len() {
            let ok = StackWalk64(
                0x8664, // IMAGE_FILE_MACHINE_AMD64
                hproc,
                hthread,
                &mut frame,
                &mut ctx as *mut _ as *mut c_void,
                None,
                Some(SymFunctionTableAccess64),
                Some(SymGetModuleBase64),
                None,
            );
            if ok == 0 || frame.AddrPC.Offset == 0 {
                break;
            }
            buf[n] = frame.AddrPC.Offset;
            n += 1;
        }
    }
    ResumeThread(hthread);
    n
}

unsafe fn symbolize(hproc: HANDLE, addr: u64, cache: &mut HashMap<u64, String>) -> String {
    if let Some(name) = cache.get(&addr) {
        return name.clone();
    }
    let mut raw = vec![0u8; size_of::<SYMBOL_INFO>() + 1024];
    let sym = raw.as_mut_ptr() as *mut SYMBOL_INFO;
    (*sym).SizeOfStruct = size_of::<SYMBOL_INFO>() as u32;
    (*sym).MaxNameLen = 1023;
    let mut displacement = 0u64;
    let name = if SymFromAddr(hproc, addr, &mut displacement, sym) != 0 {
        CStr::from_ptr(std::ptr::addr_of!((*sym).Name) as *const i8).to_string_lossy().into_owned()
    } else {
        format!("?{addr:x}")
    };
    cache.insert(addr, name.clone());
    name
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: render-profiler <html-file> [width-pt=1000] [iterations=3] [scale=1.25]");
        std::process::exit(2);
    };
    let html = std::fs::read_to_string(path).expect("could not read the HTML file");
    let width: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000.0);
    let iterations: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let scale: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1.25);

    // dbghelp finds `render_profiler.pdb` next to the executable.
    let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_string_lossy().into_owned();
    let hproc = unsafe { GetCurrentProcess() };
    unsafe {
        SymSetOptions(SYMOPT_UNDNAME);
        let search_path = std::ffi::CString::new(exe_dir).unwrap();
        SymInitialize(hproc, search_path.as_ptr() as *const u8, 1);
    }

    let render = std::thread::spawn(move || worker(html, width, scale, iterations));
    while TID.load(SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    let hthread = unsafe {
        OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, TID.load(SeqCst))
    };

    let mut buf = [0u64; 96];
    let mut samples: Vec<Sample> = Vec::with_capacity(200_000);
    while !DONE.load(SeqCst) {
        let phase = PHASE.load(SeqCst);
        let n = unsafe { sample_once(hproc, hthread, &mut buf) };
        if n > 0 {
            samples.push(Sample { phase, stack: buf[..n].to_vec() });
        }
        std::thread::sleep(Duration::from_micros(700));
    }
    render.join().unwrap();

    // If this says symtype=0, no symbols were found (see docs/PERFORMANCE.md).
    unsafe {
        let base = GetModuleHandleA(std::ptr::null()) as u64;
        let mut info: IMAGEHLP_MODULE64 = zeroed();
        info.SizeOfStruct = size_of::<IMAGEHLP_MODULE64>() as u32;
        let ok = SymGetModuleInfo64(hproc, base, &mut info);
        println!(
            "\nsymbols: ok={ok} symtype={} (3 = PDB loaded, 0 = none) pdb={:?}",
            info.SymType,
            CStr::from_ptr(info.LoadedPdbName.as_ptr() as *const i8)
        );
    }

    let mut cache = HashMap::new();
    for (index, name) in PHASES.iter().enumerate() {
        let in_phase: Vec<&Sample> = samples.iter().filter(|s| s.phase == index as u32).collect();
        if in_phase.len() < 5 {
            continue;
        }
        println!("\n=== phase {name}: {} samples ===", in_phase.len());
        let mut self_counts: HashMap<String, usize> = HashMap::new();
        let mut inclusive_counts: HashMap<String, usize> = HashMap::new();
        for sample in &in_phase {
            let names: Vec<String> =
                sample.stack.iter().map(|&addr| unsafe { symbolize(hproc, addr, &mut cache) }).collect();
            *self_counts.entry(names[0].clone()).or_default() += 1;
            let mut seen = HashSet::new();
            for n in &names {
                if seen.insert(n.clone()) {
                    *inclusive_counts.entry(n.clone()).or_default() += 1;
                }
            }
        }
        let total = in_phase.len() as f64;
        for (title, counts) in [
            ("SELF (top frame)", &self_counts),
            ("INCLUSIVE (anywhere in the stack)", &inclusive_counts),
        ] {
            let mut rows: Vec<_> = counts.iter().collect();
            rows.sort_by(|a, b| b.1.cmp(a.1));
            println!("-- {title}, top 28");
            for (name, count) in rows.into_iter().take(28) {
                let short: String = name.chars().take(110).collect();
                println!("{:5.1}%  {short}", *count as f64 * 100.0 / total);
            }
        }
    }
}
