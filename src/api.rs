// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Very basic API wrappers for filter-related stuff in CUPS, to make the memory
//! management and use of pointers easier to analyze.

use std::{
    error::Error,
    ffi::{CStr, c_int},
    fs::File,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    os::fd::{AsRawFd, IntoRawFd},
    path::Path,
    ptr::{NonNull, null_mut},
    str::FromStr,
};

use cups_filter_sys::{
    cups_mode_e_CUPS_RASTER_READ, cups_option_t, cups_page_header2_t, cups_raster_t,
    cupsFreeOptions, cupsMarkOptions, cupsParseOptions, cupsRasterClose, cupsRasterOpen,
    cupsRasterReadHeader2, cupsRasterReadPixels, ppd_choice_t, ppd_file_t, ppdClose,
    ppdErrorString, ppdFindMarkedChoice, ppdLastError, ppdMarkDefaults, ppdOpenFd,
};

pub struct PpdFile(NonNull<ppd_file_t>);

impl PpdFile {
    pub fn open_file(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let f = File::open(path)?;
        let p = unsafe { ppdOpenFd(f.into_raw_fd()) };
        if let Some(p) = NonNull::new(p) {
            Ok(Self(p))
        } else {
            let mut linenum = 0;
            let status = unsafe { ppdLastError(&mut linenum) };
            let status_str = unsafe { CStr::from_ptr(ppdErrorString(status)) };
            let status_str = status_str.to_string_lossy();

            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("PPD load failed: line {linenum}: {status_str}"),
            ))
        }
    }

    pub fn raw(&self) -> &ppd_file_t {
        unsafe { self.0.as_ref() }
    }

    pub fn raw_mut(&mut self) -> &mut ppd_file_t {
        unsafe { self.0.as_mut() }
    }

    pub fn mark_defaults(&mut self) {
        unsafe { ppdMarkDefaults(self.raw_mut()) }
    }

    pub fn mark_options(&mut self, options: &mut Options) {
        unsafe {
            cupsMarkOptions(self.raw_mut(), options.len() as c_int, options.as_mut_ptr());
        }
    }

    pub fn find_marked_choice<'s>(&'s mut self, keyword: &CStr) -> Option<PpdChoice<'s>> {
        let choice = unsafe { ppdFindMarkedChoice(self.raw_mut(), keyword.as_ptr()) };
        unsafe { choice.as_ref().map(PpdChoice) }
    }

    pub fn parse_default_marked_choice<T>(
        &mut self,
        keyword: &CStr,
    ) -> Result<Option<T>, Box<dyn Error>>
    where
        T: FromStr,
        T::Err: Error + 'static,
    {
        self.parse_optional_marked_choice(keyword, c"Default")
    }

    pub fn parse_optional_marked_choice<T>(
        &mut self,
        keyword: &CStr,
        default: &CStr,
    ) -> Result<Option<T>, Box<dyn Error>>
    where
        T: FromStr,
        T::Err: Error + 'static,
    {
        if let Some(choice) = self.find_marked_choice(keyword) {
            choice.parse_if_not(default)
        } else {
            Ok(None)
        }
    }
}

impl Drop for PpdFile {
    fn drop(&mut self) {
        unsafe {
            ppdClose(self.0.as_ptr());
        }
    }
}

pub struct PpdChoice<'a>(&'a ppd_choice_t);

impl PpdChoice<'_> {
    pub fn choice(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.0.choice.as_ptr()) }
    }

    pub fn parse_if_not<T: FromStr>(&self, default: &CStr) -> Result<Option<T>, Box<dyn Error>>
    where
        T::Err: Error + 'static,
    {
        if self.choice() == default {
            Ok(None)
        } else {
            Ok(Some(default.to_str()?.parse()?))
        }
    }
}

pub struct Options(NonNull<cups_option_t>, usize);

impl Options {
    pub fn parse(arg: &CStr) -> Self {
        let mut options: *mut cups_option_t = null_mut();
        let num_options = unsafe { cupsParseOptions(arg.as_ptr(), 0, &mut options) };

        if let Some(nn) = NonNull::new(options) {
            Self(nn, usize::try_from(num_options).unwrap())
        } else {
            Self(NonNull::dangling(), 0)
        }
    }
}

impl Drop for Options {
    fn drop(&mut self) {
        if self.1 > 0 {
            unsafe {
                cupsFreeOptions(self.1 as c_int, self.0.as_ptr());
            }
        }
    }
}

impl Deref for Options {
    type Target = [cups_option_t];

    fn deref(&self) -> &Self::Target {
        unsafe { std::slice::from_raw_parts(self.0.as_ptr(), self.1) }
    }
}

impl DerefMut for Options {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { std::slice::from_raw_parts_mut(self.0.as_ptr(), self.1) }
    }
}

pub struct Raster {
    _handle: Box<dyn AsRawFd>,
    raw: NonNull<cups_raster_t>,
}

impl Raster {
    pub fn open_file(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        Self::new(Box::new(File::open(path)?))
    }

    pub fn stdin() -> Result<Self, std::io::Error> {
        let stdin = std::io::stdin();
        Self::new(Box::new(stdin.lock()))
    }

    pub fn new(source: Box<dyn AsRawFd>) -> Result<Self, std::io::Error> {
        let ras = unsafe { cupsRasterOpen(source.as_raw_fd(), cups_mode_e_CUPS_RASTER_READ) };

        let raw = NonNull::new(ras).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "couldn't open raster stream")
        })?;
        Ok(Self {
            _handle: source,
            raw,
        })
    }

    pub fn read_header2(&mut self) -> Result<cups_page_header2_t, std::io::Error> {
        let mut header: MaybeUninit<cups_page_header2_t> = MaybeUninit::uninit();
        let r = unsafe { cupsRasterReadHeader2(self.raw.as_ptr(), header.as_mut_ptr()) };
        if r == 0 {
            // TODO: this may not have been an OS error!
            return Err(std::io::Error::last_os_error());
        }

        Ok(unsafe { header.assume_init() })
    }

    pub fn read_pixels(&mut self, buffer: &mut [u8]) -> usize {
        let r = unsafe {
            cupsRasterReadPixels(
                self.raw.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len().try_into().unwrap(),
            )
        };
        r as usize
    }
}

impl Drop for Raster {
    fn drop(&mut self) {
        unsafe { cupsRasterClose(self.raw.as_ptr()) }
    }
}
