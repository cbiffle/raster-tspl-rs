#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unsafe_op_in_unsafe_fn)] // TODO bindgen bug
#![allow(improper_ctypes)]

#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
