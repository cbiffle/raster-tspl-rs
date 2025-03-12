// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Very basic API wrappers for filter-related stuff in CUPS, to make the memory
//! management and use of pointers easier to analyze.
//!
//! This module mostly exists because many of the CUPS functions we need
//! allocate data structures, and each data structure needs a specific,
//! different function to be explicitly called to reclaim resources. So we are
//! essentially using the "newtype" pattern to grant them Drop impls.
//!
//! But, that also gives us an opportunity to write API wrappers that assume our
//! construction invariants hold.

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

/// An evaluated PPD file with mutable state for "choices."
///
/// This is a wrapper for the CUPS `ppd_file_t` type.
pub struct PpdFile(NonNull<ppd_file_t>);

impl PpdFile {
    /// Loads a PPD file from the given path on the filesystem.
    ///
    /// If loading fails for any reason --- file not found, not accessible,
    /// contents invalid, etc. --- this returns `Err`. In the case of internal
    /// CUPS errors, it makes a reasonable effort to return a useful value in
    /// the error, but no promises.
    ///
    /// The file is not kept open after this operation returns.
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

    /// Direct access to the backing CUPS type, for calling into C.
    pub fn raw(&self) -> &ppd_file_t {
        unsafe { self.0.as_ref() }
    }

    /// Direct access to the backing CUPS type, for calling into C.
    pub fn raw_mut(&mut self) -> &mut ppd_file_t {
        unsafe { self.0.as_mut() }
    }

    /// Clears all selected choices and then selects the default choices
    /// specified in the PPD file.
    pub fn mark_defaults(&mut self) {
        unsafe { ppdMarkDefaults(self.raw_mut()) }
    }

    /// Marks the options specified in a parsed text options string from the
    /// command line.
    pub fn mark_options(&mut self, options: &mut Options) {
        unsafe {
            cupsMarkOptions(self.raw_mut(), options.len() as c_int, options.as_mut_ptr());
        }
    }

    /// Searches for a marked choice named `keyword` and returns its borrowed
    /// contents, if found.
    ///
    /// If not found, returns `None`.
    pub fn find_marked_choice<'s>(&'s mut self, keyword: &CStr) -> Option<PpdChoice<'s>> {
        let choice = unsafe { ppdFindMarkedChoice(self.raw_mut(), keyword.as_ptr()) };
        unsafe { choice.as_ref().map(PpdChoice) }
    }

    /// Finds a marked choice named `keyword` and parses it into a `T`, unless
    /// its value is the exact string `"Default"`.
    ///
    /// This is a convenience wrapper around `parse_optional_marked_choice` for
    /// the very common case.
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

    /// Finds a marked option named `keyword` and parses its contents into a `T`
    /// if they do not match `default`.
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

/// Borrowed reference to a choice within a `PpdFile`.
pub struct PpdChoice<'a>(&'a ppd_choice_t);

impl PpdChoice<'_> {
    /// Returns the chosen value.
    pub fn choice(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.0.choice.as_ptr()) }
    }

    /// Attempts to parse the chosen value, if it is not equal to `default`.
    ///
    /// This returns `Ok(None)` if the value matches `default`, `Ok(Some(x))` if
    /// the value parses successfully, and `Err` otherwise (including if the
    /// value is not UTF-8, since parsing needs to go through `str`).
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

/// A parsed set of options.
///
/// This is a wrapper around the CUPS `cups_option_t` type, which is used in
/// CUPS primarily as a slice.
pub struct Options(Option<NonNull<cups_option_t>>, usize);

impl Options {
    /// Creates a new `Options` by parsing `arg`.
    ///
    /// On failure, this function (like the original `cupsParseOptions` it
    /// wraps) returns the equivalent of an empty slice.
    pub fn parse(arg: &CStr) -> Self {
        let mut options: *mut cups_option_t = null_mut();
        let num_options = unsafe { cupsParseOptions(arg.as_ptr(), 0, &mut options) };

        // cupsParseOptions sure _suggests_ that it will return 0 (length) any
        // time the `options` pointer is not modified or written as NULL, but in
        // the interest of not compromising safety unnecessarily, we'll force
        // the length to 0.
        let p = NonNull::new(options);
        Self(p, if p.is_some() { usize::try_from(num_options).unwrap() } else { 0 })
    }
}

impl Drop for Options {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            unsafe {
                cupsFreeOptions(self.1 as c_int, p.as_ptr());
            }
        }
    }
}

/// Allow the backing array to be treated as a Rust slice.
impl Deref for Options {
    type Target = [cups_option_t];

    fn deref(&self) -> &Self::Target {
        if let Some(p) = self.0 {
            unsafe { std::slice::from_raw_parts(p.as_ptr(), self.1) }
        } else {
            &[]
        }
    }
}

/// Allow the backing array to be treated as a Rust mutable slice.
impl DerefMut for Options {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if let Some(p) = self.0 {
            unsafe { std::slice::from_raw_parts_mut(p.as_ptr(), self.1) }
        } else {
            &mut []
        }
    }
}

/// A stream of raster data from an earlier filter.
///
/// This is a wrapper around the CUPS type `cups_raster_t`, but also manages the
/// lifecycle of the input source (file or stdin).
pub struct Raster {
    _handle: Box<dyn AsRawFd>,
    raw: NonNull<cups_raster_t>,
}

impl Raster {
    /// Opens a file at a given path in the filesystem, so that it can be read
    /// as raster data.
    ///
    /// This doesn't actually read anything, just opens the file.
    ///
    /// A `Raster` created in this way will automatically close the file
    /// descriptor on drop.
    pub fn open_file(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        Self::new(Box::new(File::open(path)?))
    }

    /// Starts reading stdin as raster data, which is common in filters.
    ///
    /// To ensure a consistent stream of bytes, this locks stdin, so other
    /// attempts to use stdin will wait; if performed from the same thread,
    /// they'll hang. This seemed preferable to the traditional practice of
    /// hoping it never happens.
    ///
    /// A `Raster` created in this way will unlock stdin on drop, but will _not_
    /// close it, because that'd be rude.
    pub fn stdin() -> Result<Self, std::io::Error> {
        let stdin = std::io::stdin();
        Self::new(Box::new(stdin.lock()))
    }

    /// Constructs a raster stream from anything that wraps a system file
    /// descriptor.
    ///
    /// You probably don't want to call this directly.
    fn new(source: Box<dyn AsRawFd>) -> Result<Self, std::io::Error> {
        let ras = unsafe { cupsRasterOpen(source.as_raw_fd(), cups_mode_e_CUPS_RASTER_READ) };

        let raw = NonNull::new(ras).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "couldn't open raster stream")
        })?;
        Ok(Self {
            _handle: source,
            raw,
        })
    }

    /// Reads a raster page header from the stream.
    ///
    /// Traditionally, end of stream is detected by this function failing.
    pub fn read_header(&mut self) -> Result<cups_page_header2_t, std::io::Error> {
        let mut header: MaybeUninit<cups_page_header2_t> = MaybeUninit::uninit();
        let r = unsafe { cupsRasterReadHeader2(self.raw.as_ptr(), header.as_mut_ptr()) };
        if r == 0 {
            // TODO: this may not have been an OS error!
            return Err(std::io::Error::last_os_error());
        }

        Ok(unsafe { header.assume_init() })
    }

    /// Reads a chunk of pixels from the input stream. Typically this will be
    /// called on single rows.
    ///
    /// Returns the number of pixels read.
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
