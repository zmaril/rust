pub fn small(x: u32) -> u32 {
    x + 1
}

pub fn generic<T: Default + std::fmt::Debug>() -> String {
    format!("{:?}", T::default())
}

#[inline]
pub fn hinted(x: u64) -> u64 {
    x * 3
}

pub async fn later(x: u32) -> u32 {
    small(x)
}
