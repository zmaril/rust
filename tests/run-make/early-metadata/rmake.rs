//! `-Zearly-metadata`: a crate that depends on another can start compiling on
//! its early metadata (`.early-rmeta`), written before the dependency's
//! function bodies are checked. Before code generation, it waits for the
//! dependency's full metadata, announcing `wait-metadata` and then `resume`,
//! and must build the same program as from full metadata, at any opt-level
//! (optimizing asks dependencies for MIR, which only full metadata has).
//!
//! A dependency that hasn't written its full metadata yet holds a lock on
//! `libdep.rmeta.lock`. This test holds it in its place while the full
//! metadata is hidden. If the lock is released without the full metadata
//! (the dependency failed, or crashed), the waiting crate must stop with an
//! error rather than wait.

//@ ignore-cross-compile (runs the program it builds)
//@ needs-target-std

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;

use run_make_support::{rfs, run, rustc};

fn compile(name: &str, opt: &str) -> run_make_support::Rustc {
    let mut cmd = rustc();
    cmd.input(format!("{name}.rs"))
        .crate_name(name)
        .edition("2024")
        .opt_level(opt)
        .extra_filename("-x")
        .arg("-Zearly-metadata");
    cmd
}

fn lib(name: &str, opt: &str) -> run_make_support::Rustc {
    let mut cmd = compile(name, opt);
    cmd.crate_type("lib").emit("metadata,link");
    cmd
}

/// Builds `mid` while `dep` looks unfinished: its full metadata and rlib are
/// moved to `hidden/` and their locks held. Once `mid` announces it's
/// waiting, `finish_dep` runs and the locks are released. Returns whether
/// `mid` built, and its stderr.
fn build_mid_on_early_metadata(opt: &str, finish_dep: impl FnOnce()) -> (bool, String) {
    rfs::create_dir("hidden");
    rfs::rename("libdep-x.rmeta", "hidden/libdep-x.rmeta");
    rfs::rename("libdep-x.rlib", "hidden/libdep-x.rlib");
    let locks: Vec<File> = ["libdep-x.rmeta.lock", "libdep-x.rlib.lock"]
        .into_iter()
        .map(|path| {
            let file = File::create(path).unwrap();
            file.lock().unwrap();
            file
        })
        .collect();

    let mut mid = lib("mid", opt);
    mid.extern_("dep", "libdep-x.rmeta").error_format("json").json("artifacts");
    let mut mid = mid.into_raw_command();
    mid.stderr(Stdio::piped());
    let mut child = mid.spawn().unwrap();
    let mut stderr = String::new();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    for line in &mut lines {
        let line = line.unwrap();
        let paused = line.contains(r#""emit":"wait-metadata""#);
        stderr.push_str(&line);
        stderr.push('\n');
        if paused {
            break;
        }
    }
    assert!(stderr.contains(r#""emit":"wait-metadata""#), "never paused:\n{stderr}");
    finish_dep();
    drop(locks);
    for line in lines {
        stderr.push_str(&line.unwrap());
        stderr.push('\n');
    }
    let built = child.wait().unwrap().success();
    (built, stderr)
}

fn unhide_dep() {
    rfs::rename("hidden/libdep-x.rmeta", "libdep-x.rmeta");
    rfs::rename("hidden/libdep-x.rlib", "libdep-x.rlib");
    rfs::remove_dir("hidden");
}

fn main() {
    for opt in ["0", "2"] {
        lib("dep", opt).run();
        assert!(Path::new("libdep-x.early-rmeta").exists(), "no early metadata");
        // Its locks are gone once `dep` has finished.
        assert!(!Path::new("libdep-x.rmeta.lock").exists());
        assert!(!Path::new("libdep-x.rlib.lock").exists());

        // From full metadata.
        lib("mid", opt).extern_("dep", "libdep-x.rmeta").run();
        compile("main", opt).extern_("mid", "libmid-x.rlib").output("main").run();
        let expected = run("main").stdout_utf8();

        // Given a finished dependency's metadata, as build tools that start
        // on metadata do, a crate that links finds the rlib next to it.
        compile("main", opt).extern_("mid", "libmid-x.rmeta").output("main").run();
        assert_eq!(run("main").stdout_utf8(), expected);

        // From early metadata: the full metadata comes back while `mid` waits.
        let (built, stderr) = build_mid_on_early_metadata(opt, unhide_dep);
        assert!(built, "build from early metadata failed:\n{stderr}");
        assert!(stderr.contains(r#""emit":"resume""#), "never resumed:\n{stderr}");
        compile("main", opt).extern_("mid", "libmid-x.rlib").output("main").run();
        assert_eq!(run("main").stdout_utf8(), expected);

        // From early metadata, and `dep` stops without writing full metadata.
        let (built, stderr) = build_mid_on_early_metadata(opt, || {});
        assert!(!built, "built although the dependency failed");
        assert!(
            stderr.contains("a dependency failed to compile after writing early metadata"),
            "unexpected failure:\n{stderr}"
        );
        unhide_dep();
    }
}
