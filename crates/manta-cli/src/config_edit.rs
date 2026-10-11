//! MAN-127: sets `[input].freq_correction_ppm` in an existing config file
//! while keeping every other byte, then validates the result with the real
//! loader and atomically replaces the file.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

const KEY: &str = "freq_correction_ppm";
const BOM: char = '\u{feff}';

/// Pure: returns the new text and the previous value (None when unset).
pub(crate) fn set_freq_correction_ppm(text: &str, ppm: f64) -> Result<(String, Option<f64>)> {
    ensure!(ppm.is_finite(), "{KEY} must be a finite number, got {ppm}");
    // -0.0 would print as `-0.0`.
    let ppm = if ppm == 0.0 { 0.0 } else { ppm };
    let (bom, body) = match text.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut doc: DocumentMut = body.parse().context("parsing the config file")?;
    if !doc.contains_key("input") {
        doc.insert("input", Item::Table(Table::new()));
    }
    let previous = match doc.get_mut("input") {
        Some(Item::Table(table)) => set_in_table(table, ppm)?,
        Some(Item::Value(Value::InlineTable(table))) => match table.get_mut(KEY) {
            Some(old) => replace(old, ppm)?,
            None => {
                let mut new = Value::from(ppm);
                // The space before `}` sits on the last value; move it to
                // the new one so the old value doesn't print `"h" ,`.
                if let Some((_, last)) = table.iter_mut().last() {
                    if let Some(suffix) = last.decor().suffix().cloned() {
                        last.decor_mut().set_suffix("");
                        new.decor_mut().set_suffix(suffix);
                    }
                }
                table.insert(KEY, new);
                None
            }
        },
        _ => bail!("[input] is not a table, so {KEY} cannot be set in it"),
    };
    let mut out = doc.to_string();
    if bom {
        out.insert(0, BOM);
    }
    Ok((out, previous))
}

fn set_in_table(table: &mut Table, ppm: f64) -> Result<Option<f64>> {
    match table.get_mut(KEY) {
        Some(Item::Value(old)) => replace(old, ppm),
        Some(_) => bail!("[input].{KEY} is not a number"),
        None => {
            table.insert(KEY, Item::Value(Value::from(ppm)));
            Ok(None)
        }
    }
}

/// Swaps `old` for `ppm`, keeping its surrounding whitespace and trailing
/// comment (a plain assignment would drop them).
fn replace(old: &mut Value, ppm: f64) -> Result<Option<f64>> {
    let previous = match old {
        Value::Float(f) => *f.value(),
        Value::Integer(i) => *i.value() as f64,
        _ => bail!("[input].{KEY} is not a number"),
    };
    let decor = old.decor().clone();
    let mut new = Value::from(ppm);
    *new.decor_mut() = decor;
    *old = new;
    Ok(Some(previous))
}

/// The value as `set_freq_correction_ppm` writes it, e.g. `-1.42`.
fn literal(ppm: f64) -> String {
    let ppm = if ppm == 0.0 { 0.0 } else { ppm };
    Value::from(ppm).to_string().trim().to_string()
}

/// Removes the temp file unless the save completed.
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Saves to the file at `path` (following symlinks), returns the previous value.
pub(crate) fn save_freq_correction_ppm(path: &Path, ppm: f64) -> Result<Option<f64>> {
    let target = fs::canonicalize(path)
        .with_context(|| format!("cannot resolve config file {}", path.display()))?;
    let bytes =
        fs::read(&target).with_context(|| format!("reading config file {}", target.display()))?;
    let text = String::from_utf8(bytes)
        .map_err(|e| anyhow!("config file {} is not valid UTF-8: {e}", target.display()))?;
    let (new_text, previous) =
        set_freq_correction_ppm(&text, ppm).with_context(|| target.display().to_string())?;
    let ppm = if ppm == 0.0 { 0.0 } else { ppm };
    let meta = fs::metadata(&target)
        .with_context(|| format!("reading the metadata of {}", target.display()))?;

    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("config file {} has no parent directory", target.display()))?;
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("config file {} has no file name", target.display()))?;
    let temp = dir.join(format!(
        ".{}.calibrate-{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    // Owner-only until the original's owner and mode are copied over: the
    // config can hold a receiver password or uplink credentials, and a
    // umask-default temp file would expose them to other local users.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&temp)
        .with_context(|| format!("creating temp file {}", temp.display()))?;
    let mut guard = TempGuard {
        path: temp.clone(),
        armed: true,
    };
    file.write_all(new_text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing temp file {}", temp.display()))?;
    #[cfg(unix)]
    keep_owner(&target, &meta, &temp, ppm)?;
    fs::set_permissions(&temp, meta.permissions())
        .with_context(|| format!("setting the permissions of {}", temp.display()))?;
    drop(file);

    let loaded =
        crate::config::load(Some(&temp), crate::config::Env::Ignore).with_context(|| {
            format!(
                "{} left unchanged: the edited file does not load",
                target.display()
            )
        })?;
    let got = loaded.input.shared.freq_correction_ppm;
    ensure!(
        got == Some(ppm),
        "{} left unchanged: the edited file reads {KEY} = {got:?}, expected {}",
        target.display(),
        literal(ppm)
    );
    fs::rename(&temp, &target)
        .with_context(|| format!("replacing {} with {}", target.display(), temp.display()))?;
    guard.armed = false;
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(previous)
}

#[cfg(unix)]
fn keep_owner(target: &Path, meta: &fs::Metadata, temp: &Path, ppm: f64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (uid, gid) = (meta.uid(), meta.gid());
    let temp_meta = fs::metadata(temp)
        .with_context(|| format!("reading the metadata of {}", temp.display()))?;
    if temp_meta.uid() == uid && temp_meta.gid() == gid {
        return Ok(());
    }
    std::os::unix::fs::chown(temp, Some(uid), Some(gid)).map_err(|e| {
        anyhow!(
            "cannot keep {}'s owner (uid {uid}, gid {gid}): {e}; run calibrate as that user, \
             or set {KEY} = {} by hand",
            target.display(),
            literal(ppm)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load, Env};

    fn set(text: &str, ppm: f64) -> (String, Option<f64>) {
        set_freq_correction_ppm(text, ppm).unwrap()
    }

    #[test]
    fn replaces_an_existing_value_and_keeps_its_comment() {
        let text =
            "# top\n[input]\ntype = \"kiwi\"\nfreq_correction_ppm = 0.5   # tuned\nhost = \"h\"\n";
        let (out, previous) = set(text, -1.42);
        assert_eq!(previous, Some(0.5));
        assert_eq!(
            out,
            "# top\n[input]\ntype = \"kiwi\"\nfreq_correction_ppm = -1.42   # tuned\nhost = \"h\"\n"
        );
    }

    #[test]
    fn reads_an_integer_previous_value() {
        let (out, previous) = set("[input]\nfreq_correction_ppm = 2\n", 2.5);
        assert_eq!(previous, Some(2.0));
        assert_eq!(out, "[input]\nfreq_correction_ppm = 2.5\n");
    }

    #[test]
    fn adds_the_key_under_an_existing_input_table() {
        let scaffold = include_str!("config_init.toml");
        let (out, previous) = set(scaffold, -1.42);
        assert_eq!(previous, None);
        let before: Vec<&str> = scaffold.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        let at = before.iter().position(|l| *l == "[input]").unwrap() + 1;
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(after[..at], before[..at]);
        assert_eq!(after[at], "freq_correction_ppm = -1.42");
        assert_eq!(after[at + 1..], before[at..]);
        assert_eq!(out.ends_with('\n'), scaffold.ends_with('\n'));
    }

    #[test]
    fn appends_an_input_table_when_missing() {
        let (out, previous) = set("# c\n[spot]\nallowlist = []\n", 3.0);
        assert_eq!(previous, None);
        assert_eq!(
            out,
            "# c\n[spot]\nallowlist = []\n\n[input]\nfreq_correction_ppm = 3.0\n"
        );
        let (empty, _) = set("", 3.0);
        assert_eq!(empty, "[input]\nfreq_correction_ppm = 3.0\n");
    }

    #[test]
    fn edits_an_inline_input_table() {
        let (out, previous) = set("input = { type = \"kiwi\", host = \"h\" }  # rx\n", -1.42);
        assert_eq!(previous, None);
        assert_eq!(
            out,
            "input = { type = \"kiwi\", host = \"h\", freq_correction_ppm = -1.42 }  # rx\n"
        );
        let (out, previous) = set("input = { freq_correction_ppm = 1.0 }\n", -1.42);
        assert_eq!(previous, Some(1.0));
        assert_eq!(out, "input = { freq_correction_ppm = -1.42 }\n");
    }

    #[test]
    fn edits_dotted_input_keys() {
        let text = "input.type = \"kiwi\"\ninput.host = \"h\"\n\n[spot]\nallowlist = []\n";
        let (out, previous) = set(text, -1.42);
        assert_eq!(previous, None);
        assert_eq!(
            out,
            "input.type = \"kiwi\"\ninput.host = \"h\"\ninput.freq_correction_ppm = -1.42\n\n[spot]\nallowlist = []\n"
        );
        let (out, previous) = set("input.freq_correction_ppm = 0.5 # old\n", -1.42);
        assert_eq!(previous, Some(0.5));
        assert_eq!(out, "input.freq_correction_ppm = -1.42 # old\n");
    }

    #[test]
    fn keeps_a_utf8_bom() {
        let (out, previous) = set("\u{feff}[input]\nfreq_correction_ppm = 0.5\n", -1.42);
        assert_eq!(previous, Some(0.5));
        assert_eq!(out, "\u{feff}[input]\nfreq_correction_ppm = -1.42\n");
        let (out, _) = set("[input]\n", -1.42);
        assert!(!out.starts_with('\u{feff}'));
    }

    #[test]
    fn normalises_negative_zero() {
        let (out, _) = set("[input]\n", -0.0);
        assert_eq!(out, "[input]\nfreq_correction_ppm = 0.0\n");
        assert_eq!(literal(-0.0), "0.0");
        assert_eq!(literal(-1.42), "-1.42");
    }

    #[test]
    fn rejects_a_non_table_input() {
        let err = set_freq_correction_ppm("input = 5\n", -1.42).unwrap_err();
        assert!(format!("{err:#}").contains("[input]"), "{err:#}");
        let err = set_freq_correction_ppm("[[input]]\n", -1.42).unwrap_err();
        assert!(format!("{err:#}").contains("[input]"), "{err:#}");
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn save_validates_with_the_loader() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "manta.toml",
            "[input]\nfreq_correction_ppm = 0.5\n",
        );
        let previous = save_freq_correction_ppm(&p, -1.42).unwrap();
        assert_eq!(previous, Some(0.5));
        let loaded = load(Some(&p), Env::Ignore).unwrap();
        assert_eq!(loaded.input.shared.freq_correction_ppm, Some(-1.42));
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "manta.toml", "[input]\n");
        assert_eq!(save_freq_correction_ppm(&p, 2.5).unwrap(), None);
        assert_eq!(entries(dir.path()), ["manta.toml"]);

        let bad_body = "[input]\nfreq_correction_ppm = 0.5\n\n[bogus]\nx = 1\n";
        let bad = write(dir.path(), "bad.toml", bad_body);
        let err = save_freq_correction_ppm(&bad, -1.42).unwrap_err();
        assert!(format!("{err:#}").contains("left unchanged"), "{err:#}");
        assert_eq!(fs::read_to_string(&bad).unwrap(), bad_body);
        assert_eq!(entries(dir.path()), ["bad.toml", "manta.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "manta.toml", "[input]\n");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        save_freq_correction_ppm(&p, -1.42).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_through_a_symlink_edits_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let target = write(
            &real_dir,
            "manta.toml",
            "[input]\nfreq_correction_ppm = 1\n",
        );
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(save_freq_correction_ppm(&link, -1.42).unwrap(), Some(1.0));
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "[input]\nfreq_correction_ppm = -1.42\n"
        );
        assert_eq!(entries(&real_dir), ["manta.toml"]);
        assert_eq!(entries(dir.path()), ["link.toml", "real"]);
    }
}
