pub fn poll<F: std::future::Future>(f: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

pub fn run() -> String {
    format!(
        "{} {} {} {}",
        dep::small(1),
        dep::generic::<u32>(),
        dep::hinted(2),
        poll(dep::later(4))
    )
}
