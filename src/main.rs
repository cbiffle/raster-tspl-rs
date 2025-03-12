// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod api;

use std::{error::Error, ffi::{c_int, CString}, os::unix::ffi::OsStrExt, process::exit, sync::{atomic::{AtomicBool, Ordering}, Arc}};
use std::io::Write;

use api::{Options, PpdFile, Raster};
use cups_filter_sys::cups_page_header2_t;

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

    let mut ras = if let Some(filename) = args.get(6) {
        Raster::open_file(filename)?
    } else {
        Raster::stdin()?
    };

    // Register a signal handler to let us know if we get cancelled.
    let cancelled: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, cancelled.clone())?;

    // Open the PPD file and apply options
    let mut options = {
        let args_c = CString::new(args[5].as_bytes())?;
        Options::parse(&args_c)
    };
    let mut ppd = PpdFile::open_file(std::env::var("PPD")?)?;

    PpdFile::mark_defaults(&mut ppd);
    PpdFile::mark_options(&mut ppd, &mut options);

    setup(&ppd)?;

    let mut page = 0;
    loop {
        let Ok(header) = ras.read_header2() else {
            break;
        };

        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        page += 1;

        start_page(&mut ppd, &header)?;

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
            let r = ras.read_pixels(&mut buffer);
            if r == 0 {
                break;
            }

            output_line(&ppd, &header, y, &buffer)?;
        }

        eprintln!("INFO: finished page {page}");

        end_page(&ppd, &header)?;

        if cancelled.load(Ordering::Relaxed) {
            break;
        }
    }

    if page == 0 {
        return Err("no pages were found.".into());
    }

    Ok(())
}

const BEEPRT: c_int = 37155;

fn setup(ppd: &PpdFile) -> Result<(), Box<dyn Error>> {
    match ppd.raw().model_number {
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

fn start_page(ppd: &mut PpdFile, header: &cups_page_header2_t) -> Result<(), Box<dyn Error>> {
    match ppd.raw().model_number {
        BEEPRT => {
            let dots_per_mm_x = (10 * header.HWResolution[0]).div_ceil(254);
            let dots_per_mm_y = (10 * header.HWResolution[1]).div_ceil(254);

            let width_mm = header.cupsWidth.div_ceil(dots_per_mm_x);
            let height_mm = header.cupsHeight.div_ceil(dots_per_mm_y);

            out!("SIZE {width_mm} mm,{height_mm} mm");

            let reference_x = ppd.parse_default_marked_choice(c"AdjustHoriaontal")?
                .unwrap_or(0);
            let reference_y = ppd.parse_default_marked_choice(c"AdjustVertical")?
                .unwrap_or(0);
            let rotate = ppd.parse_default_marked_choice(c"Rotate")?
                .unwrap_or(0);

            let mut media_tracking = MediaTracking::Gap;
            if let Some(choice) = ppd.find_marked_choice(c"zeMediaTracking") {
                if choice.choice() == c"BLine" {
                    media_tracking = MediaTracking::BLine;
                } else if choice.choice() == c"Continuous" {
                    media_tracking = MediaTracking::Continuous;
                }
            }

            let gap_mark_height = ppd.parse_default_marked_choice(c"GapOrMarkHeight")?
                .unwrap_or(3);
            let gap_mark_offset = ppd.parse_default_marked_choice(c"GapOrMarkOffset")?
                .unwrap_or(0);
            let feed_offset = ppd.parse_default_marked_choice(c"FeedOffset")?
                .unwrap_or(0);
            let darkness = ppd.parse_default_marked_choice(c"Darkness")?
                .unwrap_or(8);
            let speed = ppd.parse_default_marked_choice(c"zePrintRate")?
                .unwrap_or(4);
            let autodotted = ppd.parse_default_marked_choice(c"Autodotted")?
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
    Ok(())
}

fn output_line(ppd: &PpdFile, _header: &cups_page_header2_t, _y: u32, buffer: &[u8]) -> Result<(), Box<dyn Error>> {
    match ppd.raw().model_number {
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

fn end_page(ppd: &PpdFile, _header: &cups_page_header2_t) -> Result<(), Box<dyn Error>> {
    match ppd.raw().model_number {
        BEEPRT => {
            out!("\r\nPRINT 1,1");
        }
        x => unimplemented!("model number {x}"),
    }
    Ok(())
}
