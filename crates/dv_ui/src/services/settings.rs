//! dv's settings (PLAN §11 `Settings`): the theme's palette, the window material, the light/dark
//! mode, the panel's width and the language servers' commands, kept in `<data dir>/settings.json`
//! as `{"v": 1, "palette": …, "material": …, "mode": …, "panel_width": …, "lsp": …}` (`palette`
//! and `panel_width` only once they were changed, `lsp` only when the file has it: the reviewer
//! writes it by hand, PLAN §18.4). The palette has a key of its own: the first implementation's
//! `theme` (`"Aura Dark"`) is one of the keys kept as they are.
//!
//! The default data dir is `%APPDATA%\dv` on Windows, which also holds the first
//! implementation's `settings.json`, so reading is lenient: a missing key takes its default, an
//! unreadable value is reported and replaced by its default, a file that isn't a JSON object is
//! reported and ignored, and keys this version doesn't know are kept and written back unchanged
//! (the first implementation's keys, a newer dv's). A change emits [`SettingsEvent::Changed`] at
//! once and is written in the background through a temporary file and a rename, so a crash never
//! leaves half a file; writes are ordered by generation, so the last change always wins, and
//! whatever is still unwritten when the app quits is written synchronously then
//! ([`super::persist`]).
//!
//! **Edits by hand while dv runs.** The reviewer edits `lsp` (and keeps other programs' keys)
//! in the file while dv runs, so every write reads the file again first and takes `lsp` and the
//! keys dv doesn't know from it, writing only the keys dv owns (`palette`, `material`, `mode`,
//! `panel_width`) from memory ([`write_merged`]); a file that isn't a JSON object at that
//! moment (an edit saved half-way) is left alone and the write fails, saying why. `lsp` is also
//! read again on its own ([`Settings::reload_lsp`]: **Check again** on a missing server, and
//! the window coming back to the front), so an edit reaches the language servers without a
//! restart.

use std::path::{Path, PathBuf};

use gpui::{AppContext as _, Context, EventEmitter, Subscription, Task};
use serde::Serialize;
use serde_json::{Map, Value};

use super::persist::Writer;
use crate::theme::{Material, Palette};

/// The settings file in the data dir.
pub const SETTINGS_FILE: &str = "settings.json";
/// The `"v"` this version writes.
pub const SETTINGS_VERSION: u64 = 1;
/// The panel's width when the reviewer never changed it (DESIGN §2).
pub const PANEL_WIDTH_DEFAULT: f32 = 280.;
/// The narrowest and widest the panel can be dragged (DESIGN §2).
pub const PANEL_WIDTH_RANGE: (f32, f32) = (220., 420.);

/// `width` within [`PANEL_WIDTH_RANGE`]; the default for anything that isn't a finite number.
pub fn clamp_panel_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(PANEL_WIDTH_RANGE.0, PANEL_WIDTH_RANGE.1)
    } else {
        PANEL_WIDTH_DEFAULT
    }
}

/// Light or dark (DESIGN §1.1, §1.2), or whichever the OS uses: `gpui_appearance`'s, spelled
/// `"system"`, `"dark"` or `"light"` in the file.
pub use gpui_appearance::Mode;

/// The settings file's contents.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsFile {
    /// The theme's family (DESIGN §1.3); `None` until it is chosen, which is Aura.
    pub palette: Option<Palette>,
    /// The material asked for; the platform may show Opaque instead (`crate::appearance`).
    pub material: Material,
    pub mode: Mode,
    /// The panel's width in logical pixels, within [`PANEL_WIDTH_RANGE`]; `None` until the
    /// reviewer resizes it ([`PANEL_WIDTH_DEFAULT`]).
    pub panel_width: Option<f32>,
    /// `lsp` as written (`{"typescript": {"command": ["/x/tsc", "--lsp", "--stdio"]}}`):
    /// written back as it was read, its keys dv doesn't know included; read leniently through
    /// [`SettingsFile::lsp_settings`] (PLAN §18.4).
    pub lsp: Option<Value>,
    /// Keys this version doesn't know, written back as they were read.
    extra: Map<String, Value>,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            palette: None,
            material: crate::appearance::default_material(),
            mode: Mode::default(),
            panel_width: None,
            lsp: None,
            extra: Map::new(),
        }
    }
}

/// The file as written: `v` and the known keys first, then the kept ones.
#[derive(Serialize)]
struct Written<'a> {
    v: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    palette: Option<Palette>,
    material: Material,
    mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    panel_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lsp: Option<&'a Value>,
    #[serde(flatten)]
    extra: &'a Map<String, Value>,
}

impl SettingsFile {
    /// Reads `text` leniently (see the module docs): the settings, and a line for each thing
    /// that couldn't be read.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut problems = Vec::new();
        let mut file = Self::default();
        let mut object = match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => object,
            Ok(_) => {
                problems.push("the file isn't a JSON object; using the defaults".to_string());
                return (file, problems);
            }
            Err(error) => {
                problems.push(format!("the file isn't JSON ({error}); using the defaults"));
                return (file, problems);
            }
        };
        if let Some(v) = object.remove("v") {
            match v.as_u64() {
                Some(v) if v > SETTINGS_VERSION => problems.push(format!(
                    "written by a newer dv (v {v}); reading the keys this version knows"
                )),
                Some(_) => {}
                None => problems.push(format!("\"v\" is {v}, not a version number")),
            }
        }
        if let Some(value) = object.remove("palette") {
            match serde_json::from_value(value.clone()) {
                Ok(palette) => file.palette = Some(palette),
                Err(_) => problems.push(format!(
                    "\"palette\" is {value}, not one of \"aura\", \"claude\", \"dracula\"; \
                     using \"aura\""
                )),
            }
        }
        if let Some(value) = object.remove("material") {
            match serde_json::from_value(value.clone()) {
                Ok(material) => file.material = material,
                Err(_) => problems.push(format!(
                    "\"material\" is {value}, not one of \"mica\", \"acrylic\", \"opaque\"; \
                     using {:?}",
                    file.material.key()
                )),
            }
        }
        if let Some(value) = object.remove("mode") {
            match serde_json::from_value(value.clone()) {
                Ok(mode) => file.mode = mode,
                Err(_) => problems.push(format!(
                    "\"mode\" is {value}, not one of \"system\", \"dark\", \"light\"; using \
                     \"system\""
                )),
            }
        }
        if let Some(value) = object.remove("panel_width") {
            match value.as_f64() {
                Some(width) => file.panel_width = Some(clamp_panel_width(width as f32)),
                None => problems.push(format!(
                    "\"panel_width\" is {value}, not a number; using {PANEL_WIDTH_DEFAULT}"
                )),
            }
        }
        if let Some(value) = object.remove("lsp") {
            if !value.is_object() {
                problems.push(format!(
                    "\"lsp\" is {value}, not an object of language servers; ignoring it"
                ));
            }
            file.lsp = Some(value);
        }
        file.extra = object;
        (file, problems)
    }

    /// The file's text: pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let written = Written {
            v: SETTINGS_VERSION,
            palette: self.palette,
            material: self.material,
            mode: self.mode,
            panel_width: self.panel_width,
            lsp: self.lsp.as_ref(),
            extra: &self.extra,
        };
        let mut text = serde_json::to_string_pretty(&written)
            .unwrap_or_else(|error| unreachable!("settings always serialize: {error}"));
        text.push('\n');
        text
    }

    /// `lsp` as the language servers' resolvers read it (`lsp.<id>.command`, PLAN §18.4): a
    /// value of the wrong shape is ignored, as `dv_lsp::LspSettings` reads it.
    pub fn lsp_settings(&self) -> dv_lsp::LspSettings {
        self.lsp
            .as_ref()
            .map(dv_lsp::LspSettings::from_json)
            .unwrap_or_default()
    }

    /// `update.feed` as written by hand: the folder (or URL) of `vpk pack` output an installed
    /// dv updates from instead of the one its build names (PLAN D9, §13.3); `Some("")` turns
    /// updates off. Read only: dv keeps the key as it is, like every key it doesn't own.
    pub fn update_feed(&self) -> Option<String> {
        self.extra
            .get("update")?
            .get("feed")?
            .as_str()
            .map(|feed| feed.trim().to_string())
    }

    /// The keys kept from the file that this version doesn't know.
    pub fn unknown_keys(&self) -> impl Iterator<Item = &str> {
        self.extra.keys().map(String::as_str)
    }
}

/// What [`load`] read.
#[derive(Debug)]
pub struct Loaded {
    pub file: SettingsFile,
    /// Set when a file exists at the path but couldn't be read or isn't a JSON object. Saving
    /// over it would destroy whatever it holds (it may be another version's, or locked by
    /// another process for a moment), so this session doesn't save; the text says why.
    pub unsaveable: Option<String>,
}

/// Reads the settings at `path`: the defaults when there is no file, and when it can't be read
/// or parsed (logged as a warning, like every problem inside it).
pub fn load(path: &Path) -> Loaded {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                file: SettingsFile::default(),
                unsaveable: None,
            };
        }
        Err(error) => {
            log::warn!(
                "couldn't read {}: {error}; using the default settings",
                path.display()
            );
            return Loaded {
                file: SettingsFile::default(),
                unsaveable: Some(format!(
                    "{} couldn't be read ({error}), so changes aren't saved this session",
                    path.display()
                )),
            };
        }
    };
    let unsaveable = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(_)) => None,
        _ => Some(format!(
            "{} isn't a settings file dv can read, so it's left as it is and changes aren't \
             saved this session",
            path.display()
        )),
    };
    let (file, problems) = SettingsFile::parse(&text);
    for problem in problems {
        log::warn!("{}: {problem}", path.display());
    }
    Loaded { file, unsaveable }
}

/// What [`Settings`] tells its subscribers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsEvent {
    /// A value changed.
    Changed,
}

/// The settings service: the values views and `crate::appearance` read, saving as they change.
pub struct Settings {
    file: SettingsFile,
    writer: Writer,
    /// Bumped by every change; the writer keeps the newest.
    generation: u64,
    /// The newest generation a background write has tried (written, skipped or failed).
    attempted: u64,
    /// The background write in flight; a change while it runs is written after it.
    saving: Option<Task<()>>,
    /// The last write's failure, until a write succeeds.
    last_error: Option<String>,
    /// Why this session never saves, when the existing file couldn't be read ([`Loaded`]).
    unsaveable: Option<String>,
    _quit: Subscription,
}

impl EventEmitter<SettingsEvent> for Settings {}

impl Settings {
    /// Reads `path` (see [`load`]) synchronously, so the first frame already uses the settings.
    pub fn load(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let Loaded { file, unsaveable } = load(&path);
        let quit = cx.on_app_quit(|this: &mut Self, _cx| {
            this.flush();
            async {}
        });
        Self {
            file,
            writer: Writer::new(path),
            generation: 0,
            attempted: 0,
            saving: None,
            last_error: None,
            unsaveable,
            _quit: quit,
        }
    }

    pub fn file(&self) -> &SettingsFile {
        &self.file
    }

    /// The theme's family: Aura until another is chosen.
    pub fn palette(&self) -> Palette {
        self.file.palette.unwrap_or_default()
    }

    pub fn material(&self) -> Material {
        self.file.material
    }

    pub fn mode(&self) -> Mode {
        self.file.mode
    }

    /// The panel's width in logical pixels (DESIGN §2: 220–420, 280 until resized).
    pub fn panel_width(&self) -> f32 {
        self.file.panel_width.unwrap_or(PANEL_WIDTH_DEFAULT)
    }

    /// Where the settings are saved.
    pub fn path(&self) -> &Path {
        self.writer.path()
    }

    /// Why the last save failed, until one succeeds.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// No save is waiting or running (a failed one counts as done): automation's `settled`.
    pub fn is_settled(&self) -> bool {
        self.saving.is_none() && self.attempted >= self.generation
    }

    pub fn set_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        if self.file.palette != Some(palette) {
            self.file.palette = Some(palette);
            self.changed(cx);
        }
    }

    pub fn set_material(&mut self, material: Material, cx: &mut Context<Self>) {
        if self.file.material != material {
            self.file.material = material;
            self.changed(cx);
        }
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.file.mode != mode {
            self.file.mode = mode;
            self.changed(cx);
        }
    }

    /// Sets the panel's width, clamped to [`PANEL_WIDTH_RANGE`] (a drag past either end stops
    /// there).
    pub fn set_panel_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = clamp_panel_width(width);
        if self.file.panel_width != Some(width) {
            self.file.panel_width = Some(width);
            self.changed(cx);
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        cx.emit(SettingsEvent::Changed);
        cx.notify();
        self.save(cx);
    }

    /// Starts writing the current values unless a write is running; that one starts the next
    /// when it ends, if anything changed meanwhile.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving.is_some() {
            return;
        }
        if let Some(reason) = &self.unsaveable {
            self.attempted = self.generation;
            self.last_error = Some(reason.clone());
            cx.notify();
            return;
        }
        let (writer, generation, file) = (self.writer.clone(), self.generation, self.file.clone());
        let write = cx.background_spawn(async move { write_merged(&writer, generation, file) });
        self.saving = Some(cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.saving = None;
                this.attempted = generation;
                match result {
                    Ok(merged) => {
                        this.last_error = None;
                        // What the file holds besides dv's own keys, as it was written.
                        this.file.lsp = merged.lsp;
                        this.file.extra = merged.extra;
                    }
                    Err(error) => {
                        log::warn!(
                            "couldn't save the settings to {}: {error}",
                            this.writer.path().display()
                        );
                        this.last_error = Some(error.to_string());
                    }
                }
                if this.attempted < this.generation {
                    this.save(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Writes whatever isn't written yet, on this thread (the app is quitting).
    fn flush(&mut self) {
        if self.unsaveable.is_some() || self.writer.written() >= self.generation {
            return;
        }
        if let Err(error) = write_merged(&self.writer, self.generation, self.file.clone()) {
            log::warn!(
                "couldn't save the settings to {}: {error}",
                self.writer.path().display()
            );
        }
    }

    /// Reads `lsp` (and the keys dv doesn't know) from the file again (module docs, "Edits by
    /// hand while dv runs"): the language servers' settings as the file has them now, `None`
    /// when it can't be read as settings just now. Always the file's, not only when they
    /// differ from memory: a write by dv takes the file's `lsp` into memory too, and the
    /// servers (`LanguageServers::set_settings`, which does nothing for the same settings)
    /// still need to hear of it.
    pub fn reload_lsp(&mut self) -> Option<dv_lsp::LspSettings> {
        let text = match std::fs::read_to_string(self.writer.path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::from("{}"),
            Err(error) => {
                log::info!("reading {} again: {error}", self.writer.path().display());
                return None;
            }
        };
        if !matches!(serde_json::from_str::<Value>(&text), Ok(Value::Object(_))) {
            return None;
        }
        let (disk, _) = SettingsFile::parse(&text);
        self.file.extra = disk.extra;
        self.file.lsp = disk.lsp;
        Some(self.file.lsp_settings())
    }
}

/// Writes `file` as generation `generation`, its `lsp` and the keys dv doesn't know taken
/// from the file on disk first, so dv writes only the keys it owns (module docs, "Edits by hand
/// while dv runs"): what was written. A file there that isn't a JSON object is left alone.
fn write_merged(
    writer: &Writer,
    generation: u64,
    mut file: SettingsFile,
) -> std::io::Result<SettingsFile> {
    match std::fs::read_to_string(writer.path()) {
        Ok(text) => {
            if !matches!(serde_json::from_str::<Value>(&text), Ok(Value::Object(_))) {
                return Err(std::io::Error::other(
                    "the file on disk isn't a JSON object just now (an edit saved half-way?), \
                     so it's left as it is",
                ));
            }
            let (disk, _) = SettingsFile::parse(&text);
            file.lsp = disk.lsp;
            file.extra = disk.extra;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    writer.write(generation, &file.to_json())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_missing_or_empty_object_gives_the_defaults() {
        let (file, problems) = SettingsFile::parse("{}");
        assert_eq!(file, SettingsFile::default());
        assert!(problems.is_empty());
        assert_eq!(file.mode, Mode::System);
        assert_eq!(file.material, crate::appearance::default_material());
    }

    #[test]
    fn the_first_implementations_file_keeps_its_keys() {
        let old = r#"{
          "theme": "Aura Dark",
          "follow_os_appearance": false,
          "sidebar_width": 277.33334,
          "sidebar_filters": {"pr_open": true}
        }"#;
        let (mut file, problems) = SettingsFile::parse(old);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(file.mode, Mode::System);
        file.material = Material::Acrylic;
        file.mode = Mode::Light;
        let written: Value = serde_json::from_str(&file.to_json()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "v": 1,
                "material": "acrylic",
                "mode": "light",
                "theme": "Aura Dark",
                "follow_os_appearance": false,
                "sidebar_width": 277.33334,
                "sidebar_filters": {"pr_open": true}
            })
        );
        // And it reads back as written.
        let (again, problems) = SettingsFile::parse(&file.to_json());
        assert!(problems.is_empty());
        assert_eq!(again, file);
    }

    #[test]
    fn the_palette_is_written_once_chosen_beside_the_first_implementations_theme() {
        let (mut file, problems) = SettingsFile::parse(r#"{"theme": "Claude Light"}"#);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(file.palette, None);
        let written: Value = serde_json::from_str(&file.to_json()).unwrap();
        assert!(written.get("palette").is_none(), "{written}");
        file.palette = Some(Palette::Dracula);
        let written: Value = serde_json::from_str(&file.to_json()).unwrap();
        assert_eq!(written["palette"], "dracula");
        assert_eq!(written["theme"], "Claude Light");
        let (again, _) = SettingsFile::parse(&file.to_json());
        assert_eq!(again, file);

        let (file, problems) = SettingsFile::parse(r#"{"palette": "Aura Dark"}"#);
        assert_eq!(file.palette, None);
        assert!(
            problems[0].contains("\"palette\" is \"Aura Dark\""),
            "{problems:?}"
        );
    }

    #[test]
    fn bad_values_fall_back_one_by_one() {
        let (file, problems) =
            SettingsFile::parse(r#"{"v": 1, "material": "glass", "mode": "light"}"#);
        assert_eq!(file.material, crate::appearance::default_material());
        assert_eq!(file.mode, Mode::Light);
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains("\"material\" is \"glass\""),
            "{problems:?}"
        );
        assert_eq!(file.unknown_keys().count(), 0, "a known key is never kept");

        let (file, problems) = SettingsFile::parse(r#"{"mode": 3, "v": "one"}"#);
        assert_eq!(file.mode, Mode::System);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn lsp_is_read_leniently_and_written_back_as_it_was() {
        let text = r#"{"lsp": {"typescript": {"command": ["/opt/tsc", "--lsp", "--stdio"],
            "later": true}, "css": {"command": "a shell string"}, "runtime": "bun"}}"#;
        let (file, problems) = SettingsFile::parse(text);
        assert!(problems.is_empty(), "{problems:?}");
        let lsp = file.lsp_settings();
        assert_eq!(
            lsp.command("typescript"),
            Some(&["/opt/tsc".to_string(), "--lsp".into(), "--stdio".into()][..])
        );
        assert_eq!(lsp.command("css"), None);
        assert_eq!(file.unknown_keys().count(), 0);
        let written: Value = serde_json::from_str(&file.to_json()).unwrap();
        assert_eq!(
            written["lsp"],
            serde_json::json!({
                "typescript": {"command": ["/opt/tsc", "--lsp", "--stdio"], "later": true},
                "css": {"command": "a shell string"},
                "runtime": "bun"
            })
        );
        // Without one nothing is written; one of the wrong shape is kept, read as nothing.
        assert!(!SettingsFile::default().to_json().contains("lsp"));
        let (file, problems) = SettingsFile::parse(r#"{"lsp": [1]}"#);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(file.lsp_settings(), dv_lsp::LspSettings::default());
        assert!(file.to_json().contains("\"lsp\""));
    }

    #[test]
    fn unreadable_files_give_the_defaults() {
        for text in ["", "not json", "[1, 2]", "\"mica\""] {
            let (file, problems) = SettingsFile::parse(text);
            assert_eq!(file, SettingsFile::default(), "{text:?}");
            assert_eq!(problems.len(), 1, "{text:?}");
        }
    }

    #[test]
    fn a_newer_version_is_read_as_far_as_it_goes() {
        let (file, problems) = SettingsFile::parse(r#"{"v": 7, "mode": "dark", "later_key": [1]}"#);
        assert_eq!(file.mode, Mode::Dark);
        assert_eq!(file.unknown_keys().collect::<Vec<_>>(), ["later_key"]);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("newer dv"));
    }

    #[test]
    fn the_update_feed_is_read_from_a_key_dv_keeps_as_written() {
        let (file, _) = SettingsFile::parse(r#"{"update": {"feed": " D:\\releases ", "x": 1}}"#);
        assert_eq!(file.update_feed().as_deref(), Some(r"D:\releases"));
        let written: Value = serde_json::from_str(&file.to_json()).unwrap();
        assert_eq!(
            written["update"],
            serde_json::json!({"feed": " D:\\releases ", "x": 1})
        );
        for text in ["{}", r#"{"update": 3}"#, r#"{"update": {"feed": 3}}"#] {
            assert_eq!(SettingsFile::parse(text).0.update_feed(), None, "{text}");
        }
        let (off, _) = SettingsFile::parse(r#"{"update": {"feed": ""}}"#);
        assert_eq!(off.update_feed().as_deref(), Some(""));
    }

    #[test]
    fn the_panel_width_is_read_clamped_and_written_once_set() {
        let (file, problems) = SettingsFile::parse("{}");
        assert_eq!(file.panel_width, None);
        assert!(problems.is_empty());
        assert!(!file.to_json().contains("panel_width"));
        let (file, problems) = SettingsFile::parse(r#"{"panel_width": 300.5}"#);
        assert_eq!(file.panel_width, Some(300.5));
        assert!(problems.is_empty());
        for (text, width) in [
            (r#"{"panel_width": 9000}"#, 420.),
            (r#"{"panel_width": 10}"#, 220.),
        ] {
            assert_eq!(
                SettingsFile::parse(text).0.panel_width,
                Some(width),
                "{text}"
            );
        }
        let (file, problems) = SettingsFile::parse(r#"{"panel_width": "wide"}"#);
        assert_eq!(file.panel_width, None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        // The first implementation's `sidebar_width` is another key, kept as it is.
        let (file, _) = SettingsFile::parse(r#"{"sidebar_width": 277.3}"#);
        assert_eq!(file.panel_width, None);
        assert_eq!(file.unknown_keys().collect::<Vec<_>>(), ["sidebar_width"]);
        assert_eq!(clamp_panel_width(f32::NAN), PANEL_WIDTH_DEFAULT);
    }

    #[gpui::test]
    fn changes_are_saved_in_the_background(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        std::fs::write(&path, r#"{"theme": "Aura Dark", "mode": "dark"}"#).unwrap();
        let settings = cx.new(|cx| Settings::load(path.clone(), cx));
        settings.read_with(cx, |settings, _| {
            assert_eq!(settings.mode(), Mode::Dark);
            assert!(settings.is_settled());
        });

        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&settings, move |_, event: &SettingsEvent, _| {
                events.borrow_mut().push(*event)
            })
        });
        // A material other than the platform's default, which the file leaves in place.
        let other = if crate::appearance::default_material() == Material::Opaque {
            Material::Acrylic
        } else {
            Material::Opaque
        };
        settings.update(cx, |settings, cx| {
            settings.set_material(other, cx);
            settings.set_mode(Mode::Light, cx);
            // Setting the same value again is no change.
            settings.set_mode(Mode::Light, cx);
            settings.set_panel_width(1000., cx);
            settings.set_panel_width(420., cx);
            assert_eq!(settings.panel_width(), 420.);
            assert!(!settings.is_settled());
        });
        cx.run_until_parked();
        assert_eq!(*events.borrow(), [SettingsEvent::Changed; 3]);
        settings.read_with(cx, |settings, _| {
            assert!(settings.is_settled());
            assert_eq!(settings.last_error(), None);
        });
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({
                "v": 1,
                "material": other.key(),
                "mode": "light",
                "panel_width": 420.0,
                "theme": "Aura Dark"
            })
        );
    }

    #[gpui::test]
    fn a_file_dv_cant_read_is_never_saved_over(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        let original = "settings = { mode = \"dark\" }\n";
        std::fs::write(&path, original).unwrap();
        let settings = cx.new(|cx| Settings::load(path.clone(), cx));
        settings.update(cx, |settings, cx| settings.set_mode(Mode::Light, cx));
        cx.run_until_parked();
        settings.read_with(cx, |settings, _| {
            assert_eq!(settings.mode(), Mode::Light);
            assert!(settings.is_settled());
            assert!(
                settings
                    .last_error()
                    .is_some_and(|e| e.contains("left as it is")),
                "{:?}",
                settings.last_error()
            );
        });
        cx.update(|cx| cx.quit());
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[gpui::test]
    fn hand_edits_while_dv_runs_survive_its_writes_and_lsp_reloads(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        std::fs::write(&path, r#"{"mode": "dark"}"#).unwrap();
        let settings = cx.new(|cx| Settings::load(path.clone(), cx));
        // The reviewer writes `lsp` and another program's key by hand while dv runs.
        std::fs::write(
            &path,
            r#"{"mode": "dark", "lsp": {"typescript": {"command": ["/x/tsc", "--lsp"]}},
                "theirs": 1}"#,
        )
        .unwrap();
        // dv's own change keeps both, and its memory takes them up.
        settings.update(cx, |settings, cx| settings.set_mode(Mode::Light, cx));
        cx.run_until_parked();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({
                "v": 1,
                "material": crate::appearance::default_material().key(),
                "mode": "light",
                "lsp": {"typescript": {"command": ["/x/tsc", "--lsp"]}},
                "theirs": 1
            })
        );
        // Taken into memory by the write, it is still what reading it again gives: the servers
        // must hear of it (they started with the file as it was).
        assert_eq!(
            settings.update(cx, |settings, _| settings.reload_lsp()),
            Some(dv_lsp::LspSettings::from_json(
                &serde_json::json!({"typescript": {"command": ["/x/tsc", "--lsp"]}})
            ))
        );
        // Another edit by hand: read again, it is the servers' new settings.
        std::fs::write(
            &path,
            r#"{"mode": "light", "lsp": {"typescript": {"command": ["/y/tsc"]}}}"#,
        )
        .unwrap();
        let lsp = settings.update(cx, |settings, _| settings.reload_lsp());
        assert_eq!(
            lsp,
            Some(dv_lsp::LspSettings::from_json(
                &serde_json::json!({"typescript": {"command": ["/y/tsc"]}})
            ))
        );
        // Half-saved: left alone, nothing read.
        std::fs::write(&path, r#"{"mode": "#).unwrap();
        assert_eq!(
            settings.update(cx, |settings, _| settings.reload_lsp()),
            None
        );
        settings.update(cx, |settings, cx| settings.set_panel_width(300., cx));
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"mode": "#);
        assert!(settings.read_with(cx, |settings, _| settings.last_error().is_some()));
    }

    #[test]
    fn a_path_that_exists_but_cant_be_read_is_unsaveable() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: reading it fails with something other than
        // NotFound on every platform.
        let path = dir.path().join(SETTINGS_FILE);
        std::fs::create_dir(&path).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.file, SettingsFile::default());
        assert!(loaded.unsaveable.is_some());
        assert!(load(&dir.path().join("missing.json")).unsaveable.is_none());
    }
}
