use std::{error::Error, ffi::{c_int, CStr, CString}, fs::File, mem::MaybeUninit, os::{fd::AsRawFd, unix::ffi::OsStrExt}, process::exit, ptr::null_mut, str::FromStr, sync::{atomic::{AtomicBool, Ordering}, Arc}};
use std::io::Write;

use cups_filter_sys::{cupsFreeOptions, cupsMarkOptions, cupsParseOptions, cupsRasterClose, cupsRasterOpen, cupsRasterReadHeader2, cupsRasterReadPixels, cups_mode_e_CUPS_RASTER_READ, cups_option_t, cups_page_header2_t, ppdErrorString, ppdFindMarkedChoice, ppdLastError, ppdMarkDefaults, ppdOpenFile, ppd_choice_t, ppd_file_t};

const WHITE_THRESHOLD: u8 = 128;

/// We need to write strings to stdout to send them to the printer. The printer
/// _usually_ expects `\r\n` terminators, which are hard to achieve with
/// `println!`. So, custom macro it is:
macro_rules! out {
    ($fmt:literal $($args:tt)*) => {
        print!($fmt $($args)*);
        print!("\r\n");
        std::io::stdout().flush()?;
    }
}

fn main() {
    std::panic::set_hook(Box::new(|m| {
        eprint!("ERROR: ");
        let s: &str = if let Some(s) = m.payload().downcast_ref::<&str>() {
            s
        } else if let Some(s) = m.payload().downcast_ref::<String>() {
            s
        } else {
            eprintln!("{m}");
            return;
        };
        if let Some(loc) = m.location() {
            eprint!("at {loc}: ");
        }
        eprintln!("{s}");
    }));
    match error_main() {
        Ok(()) => (),
        Err(e) => {
            eprintln!("ERROR: {e}");
            exit(1);
        }
    }
}

fn error_main() -> Result<(), Box<dyn Error>> {
    // setbuf(stderr, NULL) is not necessary -- Rust never buffers stderr
    // without you asking for it

    let args = std::env::args_os().collect::<Vec<_>>();

    if !matches!(args.len(),  6 | 7) {
        return Err("tspl-filter-rs job-id user title copies options [file]".into());
    }

    // Open the page stream

    // Local for keeping the file alive until we're done with it.
    let input: Box<dyn AsRawFd>;

    if let Some(filename) = args.get(6) {
        input = Box::new(File::open(filename)?);
    } else {
        let stdin = std::io::stdin();
        input = Box::new(stdin.lock());
    };
    let ras = unsafe {
        cupsRasterOpen(input.as_raw_fd(), cups_mode_e_CUPS_RASTER_READ)
    };

    // Register a signal handler to let us know if we get cancelled.
    let cancelled: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, cancelled.clone())?;

    // Open the PPD file and apply options
    let mut options: *mut cups_option_t = null_mut();
    let args_c = CString::new(args[5].as_bytes())?;
    let num_options = unsafe {
        cupsParseOptions(args_c.as_ptr(), 0, &mut options)
    };
    let ppd = {
        let ppd_path = std::env::var_os("PPD").ok_or("missing PPD env var")?;
        let ppd_path_c = CString::new(ppd_path.as_bytes())?;
        unsafe {
            ppdOpenFile(ppd_path_c.as_ptr())
        }
    };
    if ppd.is_null() {
        eprintln!("ERROR: the PPD file could not be opened");
        let mut linenum = 0;
        let status = unsafe { ppdLastError(&mut linenum) };
        let status_str = unsafe { CStr::from_ptr(ppdErrorString(status)) }.to_string_lossy();
        eprintln!("DEBUG: {status_str} on line {linenum}.");
        return Err("the PPD file could not be opened".into());
    }

    unsafe {
        ppdMarkDefaults(ppd);
        cupsMarkOptions(ppd, num_options, options);
    }

    setup(ppd)?;

    let mut page = 0;
    loop {
        let mut header: MaybeUninit<cups_page_header2_t> = MaybeUninit::uninit();
        let r = unsafe {
            cupsRasterReadHeader2(ras, header.as_mut_ptr())
        };
        if r == 0 {
            break;
        }

        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        page += 1;

        let header = unsafe { header.assume_init() };

        start_page(ppd, &header)?;

        let mut buffer = vec![0; header.cupsBytesPerLine as usize];

        // Loop for each line on the page...
        for y in 0..header.cupsHeight {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            if (y & 15) == 0 {
                let pct = 100 * y / header.cupsHeight;
                eprintln!("INFO: printing page {page}, {pct}% complete.");
                eprintln!("ATTR: job-media-progress={pct}");
            }

            // Read a line of graphics
            let r = unsafe {
                cupsRasterReadPixels(ras, buffer.as_mut_ptr(), header.cupsBytesPerLine)
            };
            if r < 1 {
                break;
            }

            output_line(ppd, &header, y, &buffer)?;
        }

        eprintln!("INFO: finished page {page}");

        end_page(ppd, &header)?;

        if cancelled.load(Ordering::Relaxed) {
            break; // TODO does this mean to only break the inner loop?
        }
    }

    // Close the raster stream
    unsafe {
        cupsRasterClose(ras);
        cupsFreeOptions(num_options, options);
    }

    if page == 0 {
        return Err("no pages were found.".into());
    }

    Ok(())
}

const BEEPRT: c_int = 37155;

fn setup(ppd: *mut ppd_file_t) -> Result<(), Box<dyn Error>> {
    let ppd = unsafe { &*ppd };
    match ppd.model_number {
        BEEPRT => {
            // nothing to do here
            Ok(())
        }
        x => unimplemented!("model number {x}"),
    }
}

enum MediaTracking {
    Gap,
    BLine,
    Continuous,
}

fn start_page(ppd: *mut ppd_file_t, header: &cups_page_header2_t) -> Result<(), Box<dyn Error>> {
    let ppd = unsafe { &mut *ppd };
    match ppd.model_number {
        BEEPRT => {
            let dots_per_mm_x = (10 * header.HWResolution[0]).div_ceil(254);
            let dots_per_mm_y = (10 * header.HWResolution[1]).div_ceil(254);

            let width_mm = header.cupsWidth.div_ceil(dots_per_mm_x);
            let height_mm = header.cupsHeight.div_ceil(dots_per_mm_y);

            out!("SIZE {width_mm} mm,{height_mm} mm");

            let reference_x = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"AdjustHoriaontal", c"Default")?
                .unwrap_or(0);
            let reference_y = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"AdjustVertical", c"Default")?
                .unwrap_or(0);
            let rotate = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"Rotate", c"Default")?
                .unwrap_or(0);

            let mut media_tracking = MediaTracking::Gap;
            if let Some(choice) = ppd_find_marked_choice(ppd, c"zeMediaTracking") {
                if choice.choice() == c"BLine" {
                    media_tracking = MediaTracking::BLine;
                } else if choice.choice() == c"Continuous" {
                    media_tracking = MediaTracking::Continuous;
                }
            }

            let gap_mark_height = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"GapOrMarkHeight", c"Default")?
                .unwrap_or(3);
            let gap_mark_offset = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"GapOrMarkOffset", c"Default")?
                .unwrap_or(0);
            let feed_offset = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"FeedOffset", c"Default")?
                .unwrap_or(0);
            let darkness = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"Darkness", c"Default")?
                .unwrap_or(8);
            let speed = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"zePrintRate", c"Default")?
                .unwrap_or(4);
            let autodotted = ppd_find_marked_choice_and_parse_if_not::<i32>(ppd, c"Autodotted", c"Default")?
                .unwrap_or(0);

            out!("REFERENCE {},{}",
                dots_per_mm_x as i32 * reference_x,
                dots_per_mm_y as i32 * reference_y);
            out!("DIRECTION {rotate},0");

            match media_tracking {
                MediaTracking::Gap => {
                    out!("GAP {gap_mark_height} mm,{gap_mark_offset} mm");
                }
                MediaTracking::BLine => {
                    out!("BLINE {gap_mark_height} mm,{gap_mark_offset} mm");
                }
                MediaTracking::Continuous => {
                    out!("GAP 0 mm,0 mm");
                }
            }

            out!("OFFSET {feed_offset} mm");
            out!("DENSITY {darkness}");
            out!("SPEED {speed}");

            out!("SETC AUTODOTTED {}", if autodotted != 0 { "ON" } else { "OFF" });

            out!("SETC PAUSEKEY ON");
            out!("SETC WATERMARK OFF");
            out!("CLS");

            print!("BITMAP 0,0,{},{},1,",
                (header.cupsWidth + 7) >> 3,
                header.cupsHeight);
        }
        x => unimplemented!("model number {x}"),
    }
    // TODO original allocated a global buffer here
    // TODO original set Feed=0 here
    Ok(())
}

fn ppd_find_marked_choice<'p>(ppd: &'p mut ppd_file_t, keyword: &CStr) -> Option<PpdChoice<'p>> {
    let choice = unsafe {
        ppdFindMarkedChoice(ppd, keyword.as_ptr())
    };
    unsafe {
        choice.as_ref().map(PpdChoice)
    }
}

fn ppd_find_marked_choice_and_parse_if_not<T>(
    ppd: &mut ppd_file_t,
    keyword: &CStr,
    default: &CStr,
) -> Result<Option<T>, Box<dyn Error>>
where T: FromStr,
      T::Err: Error + 'static,
{
    if let Some(choice) = ppd_find_marked_choice(ppd, keyword) {
        choice.parse_if_not(default)
    } else {
        Ok(None)
    }
}

struct PpdChoice<'a>(&'a ppd_choice_t);

impl PpdChoice<'_> {
    fn choice(&self) -> &CStr {
        unsafe {
            CStr::from_ptr(self.0.choice.as_ptr())
        }
    }

    fn parse_if_not<T: FromStr>(&self, default: &CStr) -> Result<Option<T>, Box<dyn Error>>
        where T::Err: Error + 'static,
    {
        if self.choice() == default {
            Ok(None)
        } else {
            Ok(Some(default.to_str()?.parse()?))
        }
    }
}

fn output_line(ppd: *mut ppd_file_t, _header: &cups_page_header2_t, _y: u32, buffer: &[u8]) -> Result<(), Box<dyn Error>> {
    let ppd = unsafe { &*ppd };
    match ppd.model_number {
        BEEPRT => {
            // Convert 8-bit grayscale to 1-bit black and white
            for chunk in buffer.chunks(8) {
                let mut out = 0;
                for (i, &byte) in chunk.iter().enumerate() {
                    if byte >= WHITE_THRESHOLD {
                        out |= 1 << (7 - i);
                    }
                }
                out = !out;
                std::io::stdout().write_all(std::slice::from_ref(&out))?;
            }
            std::io::stdout().flush()?;
        }
        x => unimplemented!("model number {x}"),
    }
    Ok(())
}

fn end_page(ppd: *mut ppd_file_t, _header: &cups_page_header2_t) -> Result<(), Box<dyn Error>> {
    let ppd = unsafe { &*ppd };
    match ppd.model_number {
        BEEPRT => {
            out!("\nPRINT 1,1");
        }
        x => unimplemented!("model number {x}"),
    }
    Ok(())
}
