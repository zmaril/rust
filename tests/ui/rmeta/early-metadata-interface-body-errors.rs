//! With `-Zearly-metadata`, early metadata needs a few things from function
//! bodies: a `const fn`'s MIR, an opaque type's hidden type, an `async fn`'s
//! coroutine. An error in one of those bodies is reported as usual, and no
//! early metadata is written; it used to reach the encoder and ICE.

//@ compile-flags: -Zearly-metadata --emit=metadata --crate-type=lib
//@ edition: 2024

pub const fn wrong() -> u32 {
    "not a number" //~ ERROR mismatched types
}

pub fn opaque() -> impl Sized {
    let x: u8 = 1u16; //~ ERROR mismatched types
    x
}

pub async fn later() -> usize {
    let s = String::new();
    let r = &s;
    drop(s); //~ ERROR cannot move out of `s` because it is borrowed
    r.len()
}
