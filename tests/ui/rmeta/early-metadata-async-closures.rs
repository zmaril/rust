//! With `-Zearly-metadata`, async closures' by-move bodies are created before
//! early metadata is written, in a deterministic order, once the item they're
//! in is type-checked, which also gives a const argument they're nested in
//! its type. Asking for a closure's type before that used to ICE.

//@ check-pass
//@ compile-flags: -Zearly-metadata --emit=metadata --crate-type=lib
//@ edition: 2024

pub struct Foo<const N: usize>;

pub fn closures() -> usize {
    let c = async || 1;
    let _ = c;
    let _: Foo<{
        let _nested = async || 2;
        3
    }> = Foo;
    4
}

pub async fn returns_closure() -> impl AsyncFn() -> u32 {
    async || 5
}
