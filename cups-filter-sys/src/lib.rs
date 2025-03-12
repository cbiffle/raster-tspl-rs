#![allow(nonstandard_style)] // hey man, I didn't pick these names
#![allow(unsafe_op_in_unsafe_fn)] // TODO bindgen bug #3147
#![allow(improper_ctypes)] // TODO this is u128 standing in for long double

#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
