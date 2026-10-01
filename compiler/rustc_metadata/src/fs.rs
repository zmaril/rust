use std::path::{Path, PathBuf};
use std::{fs, io};

use rustc_data_structures::temp_dir::MaybeTempDir;
use rustc_fs_util::TempDirBuilder;
use rustc_middle::ty::TyCtxt;
use rustc_session::Session;
use rustc_session::config::{OutFileName, OutputType};
use rustc_session::output::filename_for_metadata;
use rustc_span::ErrorGuaranteed;
use rustc_structures::CrateType;

use crate::diagnostics::{
    BinaryOutputToTty, FailedCopyToStdout, FailedCreateEncodedMetadata, FailedCreateFile,
    FailedCreateTempdir, FailedWriteError,
};
use crate::{EncodedMetadata, encode_early_metadata, encode_metadata};

// FIXME(eddyb) maybe include the crate name in this?
pub const METADATA_FILENAME: &str = "lib.rmeta";

/// We use a temp directory here to avoid races between concurrent rustc processes,
/// such as builds in the same directory using the same filename for metadata while
/// building an `.rlib` (stomping over one another), or writing an `.rmeta` into a
/// directory being searched for `extern crate` (observing an incomplete file).
/// The returned path is the temporary file containing the complete metadata.
pub fn emit_wrapper_file(sess: &Session, data: &[u8], tmpdir: &Path, name: &str) -> PathBuf {
    let out_filename = tmpdir.join(name);
    let result = fs::write(&out_filename, data);

    if let Err(err) = result {
        sess.dcx().emit_fatal(FailedWriteError { filename: out_filename, err });
    }

    out_filename
}

pub fn encode_and_write_metadata(tcx: TyCtxt<'_>) -> Result<EncodedMetadata, ErrorGuaranteed> {
    let out_filename = filename_for_metadata(tcx.sess, tcx.output_filenames(()));
    // To avoid races with another rustc process scanning the output directory,
    // we need to write the file somewhere else and atomically move it to its
    // final destination, with an `fs::rename` call. In order for the rename to
    // always succeed, the temporary file needs to be on the same filesystem,
    // which is why we create it inside the output directory specifically.
    let metadata_tmpdir = TempDirBuilder::new()
        .prefix("rmeta")
        .tempdir_in(out_filename.parent().unwrap_or_else(|| Path::new("")))
        .unwrap_or_else(|err| tcx.dcx().emit_fatal(FailedCreateTempdir { err }));
    let metadata_tmpdir = MaybeTempDir::new(metadata_tmpdir, tcx.sess.opts.cg.save_temps);
    let metadata_filename = metadata_tmpdir.as_ref().join("full.rmeta");
    let metadata_stub_filename = if !tcx.sess.opts.unstable_opts.embed_metadata
        && !tcx.crate_types().contains(&CrateType::ProcMacro)
    {
        Some(metadata_tmpdir.as_ref().join("stub.rmeta"))
    } else {
        None
    };

    if tcx.needs_metadata() {
        encode_metadata(tcx, &metadata_filename, metadata_stub_filename.as_deref());
    } else {
        // Always create a file at `metadata_filename`, even if we have nothing to write to it.
        // This simplifies the creation of the output `out_filename` when requested.
        std::fs::File::create(&metadata_filename).unwrap_or_else(|err| {
            tcx.dcx().emit_fatal(FailedCreateFile { filename: &metadata_filename, err });
        });
        if let Some(metadata_stub_filename) = &metadata_stub_filename {
            std::fs::File::create(metadata_stub_filename).unwrap_or_else(|err| {
                tcx.dcx().emit_fatal(FailedCreateFile { filename: &metadata_stub_filename, err });
            });
        }
    }

    if let Some(guar) = tcx.sess.dcx().has_errors_or_delayed_bugs() {
        return Err(guar);
    }

    let _prof_timer = tcx.sess.prof.generic_activity("write_crate_metadata");

    // If the user requests metadata as output, rename `metadata_filename`
    // to the expected output `out_filename`. The match above should ensure
    // this file always exists.
    let need_metadata_file = tcx.sess.opts.output_types.contains_key(&OutputType::Metadata);
    let (metadata_filename, metadata_tmpdir) = if need_metadata_file {
        let filename = match out_filename {
            OutFileName::Real(ref path) => {
                if let Err(err) = non_durable_rename(&metadata_filename, path) {
                    tcx.dcx().emit_fatal(FailedWriteError { filename: path.to_path_buf(), err });
                }
                path.clone()
            }
            OutFileName::Stdout => {
                if out_filename.is_tty() {
                    tcx.dcx().emit_err(BinaryOutputToTty);
                } else if let Err(err) = copy_to_stdout(&metadata_filename) {
                    tcx.dcx()
                        .emit_err(FailedCopyToStdout { filename: metadata_filename.clone(), err });
                }
                metadata_filename
            }
        };
        if tcx.sess.opts.json_artifact_notifications {
            tcx.dcx().emit_artifact_notification(out_filename.as_path(), "metadata");
        }
        (filename, Some(metadata_tmpdir))
    } else {
        (metadata_filename, Some(metadata_tmpdir))
    };

    // Load metadata back to memory: codegen may need to include it in object files.
    let metadata =
        EncodedMetadata::from_path(metadata_filename, metadata_stub_filename, metadata_tmpdir)
            .unwrap_or_else(|err| {
                tcx.dcx().emit_fatal(FailedCreateEncodedMetadata { err });
            });

    Ok(metadata)
}

/// Where `-Zearly-metadata` writes early metadata for a crate whose full
/// metadata goes to `rmeta`: `libfoo-hash.rmeta` becomes `libfoo-hash.early-rmeta`.
pub fn early_metadata_path(rmeta: &Path) -> PathBuf {
    rmeta.with_extension(EARLY_METADATA_EXTENSION)
}

pub const EARLY_METADATA_EXTENSION: &str = "early-rmeta";

/// The lock file guarding `output` (see [`OutputLock`]): `libfoo-hash.rlib`
/// is guarded by `libfoo-hash.rlib.lock`.
pub fn output_lock_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// `-Zearly-metadata`: an exclusive lock on an output's lock file, taken
/// before early metadata is written and released once the output (the full
/// metadata, or the rlib) is written. A dependent compilation started on the
/// early metadata that needs the output waits for the lock, then checks that
/// the output exists. If this compilation fails or crashes first, the lock is
/// released without the output having been written, and the dependent stops.
pub struct OutputLock {
    file: fs::File,
    path: PathBuf,
}

impl OutputLock {
    fn acquire(output: &Path) -> Option<OutputLock> {
        let path = output_lock_path(output);
        let file = fs::File::create(&path).ok()?;
        file.try_lock().ok()?;
        Some(OutputLock { file, path })
    }
}

impl Drop for OutputLock {
    fn drop(&mut self) {
        // A dependent that opened the file before it's removed still gets
        // the lock; one that opens it later finds no lock file, and checks
        // the output directly.
        let _ = fs::remove_file(&self.path);
        let _ = self.file.unlock();
    }
}

/// Early metadata written by [`encode_and_write_early_metadata`], and the
/// locks held until the crate's full metadata and rlib are written.
pub struct EarlyMetadataFiles {
    pub path: PathBuf,
    metadata_lock: Option<OutputLock>,
    rlib_lock: Option<OutputLock>,
}

impl EarlyMetadataFiles {
    /// The full metadata is written: dependents waiting for it can go on.
    pub fn full_metadata_written(&mut self) {
        self.metadata_lock = None;
    }

    /// The rlib is written (or won't be): dependents waiting for it can go
    /// on.
    pub fn rlib_written(&mut self) {
        self.rlib_lock = None;
    }
}

/// Writes early metadata (`-Zearly-metadata`) next to where the full metadata
/// will go, and announces it with an `early-metadata` artifact notification.
///
/// First takes the locks dependents will wait on for the full metadata and the
/// rlib (see [`OutputLock`]), and deletes full metadata and an rlib left over
/// from an earlier compilation: from then on, one that exists is this
/// compilation's. Writes nothing if a lock can't be taken.
pub fn encode_and_write_early_metadata(tcx: TyCtxt<'_>) -> Option<EarlyMetadataFiles> {
    let OutFileName::Real(out_filename) = filename_for_metadata(tcx.sess, tcx.output_filenames(()))
    else {
        return None;
    };
    let metadata_lock = Some(OutputLock::acquire(&out_filename)?);
    let _ = fs::remove_file(&out_filename);
    let mut rlib_lock = None;
    if tcx.crate_types().contains(&CrateType::Rlib) {
        let rlib = out_filename.with_extension("rlib");
        if tcx.sess.opts.output_types.should_link() {
            rlib_lock = Some(OutputLock::acquire(&rlib)?);
        }
        let _ = fs::remove_file(rlib);
    }
    let early_filename = early_metadata_path(&out_filename);
    // Written elsewhere and renamed into place, as for full metadata.
    let metadata_tmpdir = TempDirBuilder::new()
        .prefix("rmeta")
        .tempdir_in(out_filename.parent().unwrap_or_else(|| Path::new("")))
        .unwrap_or_else(|err| tcx.dcx().emit_fatal(FailedCreateTempdir { err }));
    let metadata_tmpdir = MaybeTempDir::new(metadata_tmpdir, tcx.sess.opts.cg.save_temps);
    let tmp_filename = metadata_tmpdir.as_ref().join("early.rmeta");
    encode_early_metadata(tcx, &tmp_filename);
    if tcx.sess.dcx().has_errors_or_delayed_bugs().is_some() {
        return None;
    }
    if let Err(err) = non_durable_rename(&tmp_filename, &early_filename) {
        tcx.dcx().emit_fatal(FailedWriteError { filename: early_filename, err });
    }
    if tcx.sess.opts.json_artifact_notifications {
        tcx.dcx().emit_artifact_notification(&early_filename, "early-metadata");
    }
    Some(EarlyMetadataFiles { path: early_filename, metadata_lock, rlib_lock })
}

#[cfg(not(target_os = "linux"))]
pub fn non_durable_rename(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::rename(src, dst)
}

/// This function attempts to bypass the auto_da_alloc heuristic implemented by some filesystems
/// such as btrfs and ext4. When renaming over a file that already exists then they will "helpfully"
/// write back the source file before committing the rename in case a developer forgot some of
/// the fsyncs in the open/write/fsync(file)/rename/fsync(dir) dance for atomic file updates.
///
/// To avoid triggering this heuristic we delete the destination first, if it exists.
/// The cost of an extra syscall is much lower than getting descheduled for the sync IO.
#[cfg(target_os = "linux")]
pub fn non_durable_rename(src: &Path, dst: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(dst);
    std::fs::rename(src, dst)
}

pub fn copy_to_stdout(from: &Path) -> io::Result<()> {
    let mut reader = fs::File::open_buffered(from)?;
    let mut stdout = io::stdout();
    io::copy(&mut reader, &mut stdout)?;
    Ok(())
}
