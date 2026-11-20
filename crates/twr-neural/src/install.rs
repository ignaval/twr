//! Installing a model from its archive, already downloaded and checked against the catalog
//! (`twr voices install` does both): unpacked with the system's `tar` into a private staging
//! directory next to the models, its files checked (regular files, no links), everything
//! else of the archive dropped, the model timed on this machine ([`bench`]), and the whole
//! directory renamed into place. Only a directory with its marker ([`MARKER`]) counts as
//! installed ([`installed`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};

use crate::{models_dir, ModelSpec};

/// The file in an installed model's directory that says it is whole.
pub const MARKER: &str = ".twr-model.json";

/// How long unpacking may take.
pub const UNPACK_TIMEOUT: Duration = Duration::from_secs(600);

/// The real-time factor (seconds to make a second of speech) up to which a model keeps up
/// with playback with room to spare on this machine: above it a voice is listed as slow.
pub const FAST_RTF: f32 = 0.6;

/// What the archive may keep besides the model's files: its licence and model card.
const KEEP: &[&str] = &["LICENSE", "README.md", "MODEL_CARD"];

/// What [`MARKER`] holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    /// The model's name.
    pub model: String,
    /// The SHA-256 of the archive it came from.
    pub sha256: String,
    /// Its real-time factor on this machine with `threads` threads, when it could be timed
    /// (a Kokoro or Piper model without espeak-ng can't).
    pub rtf: Option<f32>,
    /// The threads it was timed with.
    pub threads: i32,
}

impl Installed {
    /// Whether it keeps up on this machine ([`FAST_RTF`]); `None` when not timed.
    pub fn fast(&self) -> Option<bool> {
        self.rtf.map(|r| r <= FAST_RTF)
    }
}

/// `model`'s install under `data_dir`, when it is whole: its marker names it and the
/// catalog's archive, and its files are there.
pub fn installed(data_dir: &Path, model: &ModelSpec) -> Option<Installed> {
    let dir = model.path(data_dir);
    let marker: Installed = serde_json::from_slice(&std::fs::read(dir.join(MARKER)).ok()?).ok()?;
    (marker.model == model.name
        && marker.sha256 == model.sha256
        && model.files.iter().all(|f| dir.join(f).is_file()))
    .then_some(marker)
}

/// Unpacks `archive` (the catalog's, checked) and puts `model` in place under `data_dir`,
/// replacing an older install; times it with `espeak` (see the module documentation).
pub fn install_archive(
    data_dir: &Path,
    model: &'static ModelSpec,
    archive: &Path,
    espeak: Option<&Path>,
) -> anyhow::Result<Installed> {
    let root = models_dir(data_dir);
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".unpack-")
        .tempdir_in(&root)
        .with_context(|| format!("a staging directory in {}", root.display()))?;
    untar(archive, staging.path())?;
    let unpacked = staging.path().join(model.dir);
    check_unpacked(model, &unpacked)?;
    let rtf = bench(model, &unpacked, espeak).ok();
    let marker = Installed {
        model: model.name.to_string(),
        sha256: model.sha256.to_string(),
        rtf,
        threads: crate::threads(),
    };
    std::fs::write(unpacked.join(MARKER), serde_json::to_vec_pretty(&marker)?)?;
    let target = model.path(data_dir);
    // The old install moves aside first, then the new one takes its place in one rename, then
    // the old one goes: a reader sees one or the other, whole.
    let old = staging.path().join(".old");
    if target.exists() {
        std::fs::rename(&target, &old)
            .with_context(|| format!("moving the old {} aside", target.display()))?;
    }
    if let Err(e) = std::fs::rename(&unpacked, &target) {
        if old.exists() {
            let _ = std::fs::rename(&old, &target);
        }
        return Err(e).with_context(|| format!("moving the model to {}", target.display()));
    }
    // `staging` (with the old install in it) goes on drop.
    Ok(marker)
}

/// Removes `model` under `data_dir`; whether it was there.
pub fn remove(data_dir: &Path, model: &ModelSpec) -> anyhow::Result<bool> {
    let dir = model.path(data_dir);
    if !dir.exists() {
        return Ok(false);
    }
    // Renamed out of the way first, so nothing loads a half-removed model.
    let root = models_dir(data_dir);
    let trash = tempfile::Builder::new()
        .prefix(".remove-")
        .tempdir_in(&root)?;
    std::fs::rename(&dir, trash.path().join(model.name))?;
    Ok(true)
}

/// `tar -xjf archive -C dir`, killed after [`UNPACK_TIMEOUT`]. GNU tar and bsdtar both strip a
/// leading `/` and refuse `..` in names.
fn untar(archive: &Path, dir: &Path) -> anyhow::Result<()> {
    let mut child = Command::new("tar")
        .arg("-xjf")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running tar")?;
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > UNPACK_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("unpacking took longer than {UNPACK_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut err);
        }
        bail!(
            "tar failed ({status}): {}",
            err.lines().next().unwrap_or("")
        );
    }
    Ok(())
}

/// The unpacked directory has the model's files, as regular files; everything else in it
/// goes ([`KEEP`] stays).
fn check_unpacked(model: &ModelSpec, dir: &Path) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|_| anyhow!("the archive has no {} directory", model.dir))?;
    if !meta.is_dir() {
        bail!("{} in the archive is not a directory", model.dir);
    }
    for f in model.files {
        let p = dir.join(f);
        let m = std::fs::symlink_metadata(&p)
            .map_err(|_| anyhow!("the archive has no {}/{f}", model.dir))?;
        if !m.file_type().is_file() {
            bail!("{}/{f} in the archive is not a plain file", model.dir);
        }
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if model.files.contains(&name.as_ref()) || KEEP.contains(&name.as_ref()) {
            continue;
        }
        let p: PathBuf = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// What [`bench`] says, in a model's first language.
fn bench_text(lang: &str) -> &'static str {
    match lang {
        "es" => {
            "Revisé el cambio y pasan todas las pruebas. Hay dos cosas que necesitan tu \
             atención antes de publicarlo."
        }
        _ => {
            "I reviewed the change, and every test passes. Two things still need your \
             attention before it ships."
        }
    }
}

/// `model`'s real-time factor on this machine, from the model in `dir`: a warm-up sentence,
/// then two sentences timed.
pub fn bench(model: &'static ModelSpec, dir: &Path, espeak: Option<&Path>) -> anyhow::Result<f32> {
    let voice = &model.voices[0];
    crate::synth::say_now(dir, espeak, model, voice, "Hello.")?;
    let (clips, took) =
        crate::synth::say_now(dir, espeak, model, voice, bench_text(voice.languages[0]))?;
    let secs: f32 = clips.iter().map(|c| c.secs()).sum();
    if secs <= 0.0 {
        bail!("{} said nothing", model.title);
    }
    Ok(took.as_secs_f32() / secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An archive like the release's, made with the system's tar.
    fn archive(dir: &Path, model: &ModelSpec, extra: &[(&str, &str)]) -> PathBuf {
        let src = dir.join("src");
        let top = src.join(model.dir);
        std::fs::create_dir_all(top.join("espeak-ng-data/voices")).unwrap();
        for f in model.files {
            std::fs::write(top.join(f), b"x").unwrap();
        }
        std::fs::write(top.join("LICENSE"), b"license").unwrap();
        std::fs::write(top.join("espeak-ng-data/voices/en"), b"x").unwrap();
        for (name, body) in extra {
            std::fs::write(top.join(name), body).unwrap();
        }
        let out = dir.join("model.tar.bz2");
        let status = Command::new("tar")
            .arg("-cjf")
            .arg(&out)
            .arg("-C")
            .arg(&src)
            .arg(model.dir)
            .status()
            .unwrap();
        assert!(status.success());
        out
    }

    #[test]
    fn an_archive_installs_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let m = crate::model("piper-es").unwrap();
        let a = archive(dir.path(), m, &[("extra.txt", "dropped")]);
        assert!(installed(&data, m).is_none());
        // The files are placeholders: timing fails, the install doesn't.
        let got = install_archive(&data, m, &a, None).unwrap();
        assert_eq!(got.rtf, None);
        assert_eq!(installed(&data, m), Some(got));
        let p = m.path(&data);
        assert!(p.join("LICENSE").is_file());
        assert!(!p.join("extra.txt").exists() && !p.join("espeak-ng-data").exists());
        // No staging left behind.
        let left: Vec<_> = std::fs::read_dir(models_dir(&data))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from(m.name)]);
        // Again: replaces it.
        install_archive(&data, m, &a, None).unwrap();
        assert!(installed(&data, m).is_some());
        // A marker of another archive is not this install.
        std::fs::write(
            p.join(MARKER),
            r#"{"model":"piper-es","sha256":"00","rtf":null,"threads":1}"#,
        )
        .unwrap();
        assert!(installed(&data, m).is_none());
        assert!(remove(&data, m).unwrap());
        assert!(!p.exists());
        assert!(!remove(&data, m).unwrap());
    }

    #[test]
    fn an_archive_without_the_files_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let m = crate::model("piper-es").unwrap();
        let other = crate::model("piper-en").unwrap();
        // The other model's archive: the wrong directory.
        let a = archive(dir.path(), other, &[]);
        let e = install_archive(&data, m, &a, None).unwrap_err().to_string();
        assert!(e.contains("no vits-piper-es_MX-claude-high"), "{e}");
        assert!(installed(&data, m).is_none());
        let e = install_archive(&data, m, &dir.path().join("missing.tar.bz2"), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("tar failed"), "{e}");
    }

    /// With the models in `TWR_TEST_SPEECH_MODELS` (a directory of unpacked archives, as
    /// `scripts/tts-bench.sh` in Dilo leaves them): each one says something.
    #[test]
    #[ignore = "needs the models: TWR_TEST_SPEECH_MODELS=<dir>"]
    fn the_models_speak() {
        let dir = PathBuf::from(std::env::var_os("TWR_TEST_SPEECH_MODELS").unwrap());
        let espeak = crate::phonemes::find_espeak("", std::env::var_os("PATH").as_deref());
        for m in crate::MODELS {
            let rtf = bench(m, &dir.join(m.dir), espeak.as_deref()).unwrap();
            eprintln!("{}: rtf {rtf:.3}", m.name);
            assert!(rtf > 0.0);
        }
    }
}
