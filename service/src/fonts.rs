//! The window's fonts.
//!
//! egui draws each character with the first font in the family that has
//! it. The families are egui's own fonts with three Windows fonts added:
//! Segoe UI first and Segoe UI Semibold second in the proportional family,
//! Segoe UI Symbol last in both (arrows, shapes, media symbols). After
//! those come fonts from the Windows fonts folder for the scripts Segoe UI
//! lacks: Chinese, Japanese and Korean, the rarer Han characters, Indic
//! scripts, Thai and its neighbours, Ethiopic, Cherokee, Myanmar, Tibetan,
//! Mongolian and Yi. They come last, so every character drawn before they
//! were added is drawn by the same font as before.
//!
//! Han characters are shared by the Chinese, Japanese and Korean fonts and
//! are drawn differently in each, so their order follows the person's
//! languages (see [`han_order`]); kana and hangul are in one font each.
//!
//! Nothing is bundled or downloaded: the files are Windows' own, mapped
//! read-only for the life of the process and handed to egui as borrowed
//! bytes, so each is in memory once and only the parts read are loaded. A
//! missing file is normal (some Windows editions lack some fonts) and a
//! file that egui couldn't read is skipped: egui stops the app on a font it
//! can't parse.
//!
//! Limits egui 0.29 has whatever the fonts: text is laid out one character
//! at a time from left to right, so scripts that join letters or run right
//! to left (Arabic, Hebrew, the Indic scripts, Thai) show their letters
//! unjoined, and right-to-left text in reverse order.

use std::borrow::Cow;

use egui::{FontData, FontDefinitions, FontFamily};

/// Which convention Han characters are drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Han {
    Simplified,
    Traditional,
    Japanese,
    Korean,
}

/// The order after the person's own languages, so every script still has a
/// font on a system in any other language.
pub const DEFAULT_HAN_ORDER: [Han; 4] = [Han::Simplified, Han::Traditional, Han::Japanese, Han::Korean];

/// The convention for a language tag such as `zh-TW`, `zh-Hant-HK`,
/// `ja-JP` or `ko`; `None` for a language that doesn't use Han characters.
/// For Chinese, a script subtag decides (`Hant`, `Hans`), else the region
/// (Taiwan, Hong Kong and Macau write Traditional), else Simplified.
pub fn han_for_language(tag: &str) -> Option<Han> {
    let lower = tag.to_ascii_lowercase();
    let mut parts = lower.split(['-', '_']);
    match parts.next()? {
        "ja" => Some(Han::Japanese),
        "ko" => Some(Han::Korean),
        "zh" => {
            let rest: Vec<&str> = parts.collect();
            if let Some(script) = rest.iter().find(|p| p.len() == 4) {
                return Some(if *script == "hant" { Han::Traditional } else { Han::Simplified });
            }
            let traditional = rest.iter().any(|p| matches!(*p, "tw" | "hk" | "mo"));
            Some(if traditional { Han::Traditional } else { Han::Simplified })
        }
        _ => None,
    }
}

/// The convention a Windows ANSI code page implies: 936 Simplified, 950
/// Traditional, 932 Japanese, 949 Korean.
fn han_for_code_page(code_page: u32) -> Option<Han> {
    match code_page {
        936 => Some(Han::Simplified),
        950 => Some(Han::Traditional),
        932 => Some(Han::Japanese),
        949 => Some(Han::Korean),
        _ => None,
    }
}

/// The order of the Chinese, Japanese and Korean fonts: the conventions of
/// `languages` (most preferred first: the display languages, then the
/// language of the regional format) in the order they first appear, then
/// the one the system code page implies, then [`DEFAULT_HAN_ORDER`].
pub fn han_order(languages: &[String], code_page: Option<u32>) -> Vec<Han> {
    let mut order: Vec<Han> = Vec::new();
    let found = languages.iter().filter_map(|l| han_for_language(l));
    for han in found.chain(code_page.and_then(han_for_code_page)).chain(DEFAULT_HAN_ORDER) {
        if !order.contains(&han) {
            order.push(han);
        }
    }
    order
}

/// A face in a font file: the file's name in the fonts folder, and the
/// family names to choose the face by when the file is a collection
/// (`.ttc`), most wanted first.
#[derive(Debug)]
pub struct FaceIn {
    pub file: &'static str,
    pub families: &'static [&'static str],
}

/// A font in the stack, under `name`: the first of `choices` that loads.
#[derive(Debug)]
pub struct Slot {
    pub name: &'static str,
    pub choices: &'static [FaceIn],
}

const fn slot(name: &'static str, choices: &'static [FaceIn]) -> Slot {
    Slot { name, choices }
}

const fn face(file: &'static str, families: &'static [&'static str]) -> FaceIn {
    FaceIn { file, families }
}

pub const SEGOE_UI: Slot = slot("segoe_ui", &[face("segoeui.ttf", &["Segoe UI"])]);
pub const SEGOE_UI_SEMIBOLD: Slot = slot("segoe_ui_semibold", &[face("seguisb.ttf", &["Segoe UI Semibold"])]);
pub const SEGOE_UI_SYMBOL: Slot = slot("segoe_ui_symbol", &[face("seguisym.ttf", &["Segoe UI Symbol"])]);

const SIMPLIFIED: Slot = slot("han_simplified", &[face("msyh.ttc", &["Microsoft YaHei UI", "Microsoft YaHei"])]);
const TRADITIONAL: Slot =
    slot("han_traditional", &[face("msjh.ttc", &["Microsoft JhengHei UI", "Microsoft JhengHei"])]);
const JAPANESE: Slot = slot(
    "han_japanese",
    &[face("YuGothM.ttc", &["Yu Gothic UI", "Yu Gothic"]), face("msgothic.ttc", &["MS UI Gothic", "MS Gothic"])],
);
const KOREAN: Slot = slot("han_korean", &[face("malgun.ttf", &["Malgun Gothic"])]);

/// The fonts after the Chinese, Japanese and Korean ones, in this order.
const OTHER_SCRIPTS: [Slot; 9] = [
    // Han characters beyond the common set (Extension B and later).
    slot("han_rare", &[face("simsunb.ttf", &["SimSun-ExtB"]), face("mingliub.ttc", &["MingLiU-ExtB"])]),
    slot("indic", &[face("Nirmala.ttc", &["Nirmala UI"]), face("Nirmala.ttf", &["Nirmala UI"])]),
    slot("thai_lao_khmer", &[face("LeelawUI.ttf", &["Leelawadee UI"])]),
    slot("ethiopic", &[face("ebrima.ttf", &["Ebrima"])]),
    slot("cherokee_syllabics", &[face("gadugi.ttf", &["Gadugi"])]),
    slot("myanmar", &[face("mmrtext.ttf", &["Myanmar Text"])]),
    slot("tibetan", &[face("himalaya.ttf", &["Microsoft Himalaya"])]),
    slot("mongolian", &[face("monbaiti.ttf", &["Mongolian Baiti"])]),
    slot("yi", &[face("msyi.ttf", &["Microsoft Yi Baiti"])]),
];

fn han_slot(han: Han) -> &'static Slot {
    match han {
        Han::Simplified => &SIMPLIFIED,
        Han::Traditional => &TRADITIONAL,
        Han::Japanese => &JAPANESE,
        Han::Korean => &KOREAN,
    }
}

/// The fonts added after today's, in order: the Chinese, Japanese and
/// Korean ones in `han` order, then the others.
pub fn fallback_slots(han: &[Han]) -> Vec<&'static Slot> {
    let mut slots: Vec<&'static Slot> = Vec::new();
    for h in han.iter().copied().chain(DEFAULT_HAN_ORDER) {
        let s = han_slot(h);
        if !slots.iter().any(|t| t.name == s.name) {
            slots.push(s);
        }
    }
    slots.extend(OTHER_SCRIPTS.iter());
    slots
}

/// The face to use in a font file's `bytes`: the only one in a plain font
/// file; in a collection, the first face whose family (or typographic
/// family) name is, ignoring case, the earliest of `families` found —
/// never a fixed position, since the order inside a collection isn't
/// documented. The face must parse the way egui will parse it (its font
/// library is built on the same parser). Why not, otherwise.
pub fn choose_face(bytes: &[u8], families: &[&str]) -> Result<u32, String> {
    let index = match ttf_parser::fonts_in_collection(bytes) {
        None => 0,
        Some(count) => {
            let names: Vec<(u32, Vec<String>)> = (0..count.min(64))
                .filter_map(|i| ttf_parser::Face::parse(bytes, i).ok().map(|f| (i, family_names(&f))))
                .collect();
            let found = families.iter().find_map(|want| {
                names.iter().find(|(_, have)| have.iter().any(|h| h.eq_ignore_ascii_case(want))).map(|(i, _)| *i)
            });
            match found {
                Some(i) => i,
                None => {
                    let seen: Vec<&String> = names.iter().flat_map(|(_, n)| n).collect();
                    return Err(format!("no face named {families:?} in it (it has {seen:?})"));
                }
            }
        }
    };
    ttf_parser::Face::parse(bytes, index).map(|_| index).map_err(|e| format!("can't be read as a font ({e})"))
}

/// A face's family and typographic family names, in every language the
/// font gives them.
pub fn family_names(face: &ttf_parser::Face) -> Vec<String> {
    use ttf_parser::name_id::{FAMILY, TYPOGRAPHIC_FAMILY};
    let mut out: Vec<String> = Vec::new();
    for name in face.names() {
        if name.name_id == FAMILY || name.name_id == TYPOGRAPHIC_FAMILY {
            if let Some(text) = name.to_string() {
                if !out.contains(&text) {
                    out.push(text);
                }
            }
        }
    }
    out
}

/// What [`definitions`] put in each slot, for the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    /// Slot name and the file it took.
    pub used: Vec<(&'static str, &'static str)>,
    /// Files there but not usable, and why.
    pub skipped: Vec<(&'static str, String)>,
}

/// The first choice of `slot` that `load` finds and egui can read.
fn load_slot(
    slot: &Slot,
    load: &mut dyn FnMut(&str) -> Option<&'static [u8]>,
    loaded: &mut Loaded,
) -> Option<FontData> {
    for choice in slot.choices {
        let Some(bytes) = load(choice.file) else { continue };
        match choose_face(bytes, choice.families) {
            Ok(index) => {
                loaded.used.push((slot.name, choice.file));
                return Some(FontData { font: Cow::Borrowed(bytes), index, tweak: Default::default() });
            }
            Err(why) => loaded.skipped.push((choice.file, why)),
        }
    }
    None
}

/// The window's font definitions, from the font files `load` returns by
/// name (`None` for one that isn't there): egui's defaults; Segoe UI first
/// and Segoe UI Semibold second in the proportional family; Segoe UI
/// Symbol last in both families; then each of [`fallback_slots`] for `han`
/// at the end of both families.
pub fn definitions(han: &[Han], load: &mut dyn FnMut(&str) -> Option<&'static [u8]>) -> (FontDefinitions, Loaded) {
    let mut fonts = FontDefinitions::default();
    let mut loaded = Loaded::default();
    let mut add = |fonts: &mut FontDefinitions, slot: &Slot, place: &dyn Fn(&mut Vec<String>, String), both: bool| {
        if let Some(data) = load_slot(slot, load, &mut loaded) {
            fonts.font_data.insert(slot.name.to_owned(), data);
            let families: &[FontFamily] =
                if both { &[FontFamily::Proportional, FontFamily::Monospace] } else { &[FontFamily::Proportional] };
            for family in families {
                place(fonts.families.entry(family.clone()).or_default(), slot.name.to_owned());
            }
        }
    };
    let at = |i: usize| move |list: &mut Vec<String>, name: String| list.insert(i.min(list.len()), name);
    let last = |list: &mut Vec<String>, name: String| list.push(name);
    add(&mut fonts, &SEGOE_UI, &at(0), false);
    add(&mut fonts, &SEGOE_UI_SEMIBOLD, &at(1), false);
    add(&mut fonts, &SEGOE_UI_SYMBOL, &last, true);
    for slot in fallback_slots(han) {
        add(&mut fonts, slot, &last, true);
    }
    (fonts, loaded)
}

/// Set the window's fonts: the files from the Windows fonts folder, the
/// Han order from the person's languages.
#[cfg(windows)]
pub fn install(ctx: &egui::Context) {
    let dir = windows::fonts_folder();
    let languages = windows::languages();
    let han = han_order(&languages, windows::ansi_code_page());
    let mut load = |file: &str| {
        let path = dir.join(file);
        match windows::map_read_only(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::debug!("fonts: {} isn't there", path.display());
                None
            }
            Err(e) => {
                log::warn!("fonts: couldn't open {} ({e}); skipping it", path.display());
                None
            }
        }
    };
    let (fonts, loaded) = definitions(&han, &mut load);
    for (file, why) in &loaded.skipped {
        log::warn!("fonts: skipping {file}: {why}");
    }
    let used: Vec<String> = loaded.used.iter().map(|(slot, file)| format!("{slot}={file}")).collect();
    log::info!("fonts: languages {languages:?}, Han order {han:?}; using {}", used.join(", "));
    ctx.set_fonts(fonts);
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::{Path, PathBuf};

    /// `%WINDIR%\Fonts`: the system's own Windows folder, also on a
    /// Terminal Server host, where `GetWindowsDirectoryW` can give a
    /// private one.
    pub fn fonts_folder() -> PathBuf {
        use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
        let mut buf = [0u16; 260];
        // SAFETY: the buffer and its length describe `buf`.
        let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let windows = match buf.get(..n) {
            Some(units) if n > 0 => PathBuf::from(String::from_utf16_lossy(units)),
            _ => PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into())),
        };
        windows.join("Fonts")
    }

    /// The person's display languages, most preferred first, then the
    /// language of their regional format.
    pub fn languages() -> Vec<String> {
        use windows_sys::Win32::Globalization::{
            GetUserDefaultLocaleName, GetUserPreferredUILanguages, MUI_LANGUAGE_NAME,
        };
        let mut out: Vec<String> = Vec::new();
        let (mut count, mut len) = (0u32, 0u32);
        // SAFETY: a null buffer asks for the length; the second call writes
        // at most `len` units into a buffer of that size.
        if unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, std::ptr::null_mut(), &mut len) } != 0
            && len > 0
        {
            let mut buf = vec![0u16; len as usize];
            if unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buf.as_mut_ptr(), &mut len) } != 0 {
                // A list of NUL-terminated names, ended by an empty one.
                out.extend(
                    buf.split(|&u| u == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy),
                );
            }
        }
        let mut name = [0u16; 85];
        // SAFETY: the buffer and its length describe `name`.
        let n = unsafe { GetUserDefaultLocaleName(name.as_mut_ptr(), name.len() as i32) };
        if let Some(units) = usize::try_from(n).ok().and_then(|n| name.get(..n.saturating_sub(1))) {
            if !units.is_empty() {
                out.push(String::from_utf16_lossy(units));
            }
        }
        out
    }

    /// The system's ANSI code page.
    pub fn ansi_code_page() -> Option<u32> {
        // SAFETY: no arguments.
        let cp = unsafe { windows_sys::Win32::Globalization::GetACP() };
        (cp != 0).then_some(cp)
    }

    /// The file at `path`, mapped read-only for the rest of the process.
    /// It stays open without sharing write or delete access, so nothing
    /// can change or remove it while it is mapped. Never unmapped: egui
    /// keeps the fonts for the life of the window, which is the life of
    /// the process.
    pub fn map_read_only(path: &Path) -> io::Result<&'static [u8]> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY};
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let file = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if file == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut size = 0i64;
        // SAFETY: `file` is an open handle; `size` receives the length.
        let len = if unsafe { GetFileSizeEx(file, &mut size) } != 0 { usize::try_from(size).ok() } else { None };
        let Some(len) = len.filter(|&n| n > 0) else {
            let e = io::Error::new(io::ErrorKind::InvalidData, "empty or unknown size");
            // SAFETY: closing the handle opened above.
            unsafe { CloseHandle(file) };
            return Err(e);
        };
        // SAFETY: a read-only mapping of the whole open file.
        let mapping =
            unsafe { CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null()) };
        if mapping.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: closing the handle opened above.
            unsafe { CloseHandle(file) };
            return Err(e);
        }
        // SAFETY: a read view of the whole mapping.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: closing the handles opened above.
            unsafe {
                CloseHandle(mapping);
                CloseHandle(file);
            }
            return Err(e);
        }
        // The file and mapping handles stay open on purpose: they keep the
        // view valid and the file locked against writers.
        // SAFETY: the view is `len` readable bytes, never unmapped or
        // written (no one else may open the file for writing).
        Ok(unsafe { std::slice::from_raw_parts(view.Value as *const u8, len) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_han_order_follows_the_persons_languages() {
        use Han::*;
        let first = |l: &[&str], cp: Option<u32>| han_order(&langs(l), cp)[0];
        assert_eq!(first(&["zh-TW", "en-US"], None), Traditional);
        assert_eq!(first(&["zh-CN"], None), Simplified);
        assert_eq!(first(&["zh"], None), Simplified);
        assert_eq!(first(&["zh-Hant"], None), Traditional);
        assert_eq!(first(&["zh-Hant-HK"], None), Traditional);
        assert_eq!(first(&["zh-Hans-HK"], None), Simplified, "the script decides over the region");
        assert_eq!(first(&["zh-MO"], None), Traditional);
        assert_eq!(first(&["en-US", "ja"], None), Japanese);
        assert_eq!(han_order(&langs(&["ko-KR", "ja-JP"]), None), [Korean, Japanese, Simplified, Traditional]);
        assert_eq!(first(&["en-US"], Some(950)), Traditional, "the code page as a weak hint");
        assert_eq!(first(&["ja-JP"], Some(950)), Japanese, "languages first");
        assert_eq!(han_order(&[], None), DEFAULT_HAN_ORDER);
        assert_eq!(han_order(&langs(&["en-GB", "de-DE"]), Some(1252)), DEFAULT_HAN_ORDER);
        // Every convention always has a place, once.
        for l in [&["zh-TW", "zh-TW", "ja"][..], &["ko"], &[]] {
            let order = han_order(&langs(l), Some(936));
            assert_eq!(order.len(), 4, "{l:?}");
        }
    }

    #[test]
    fn the_issues_machine_gets_traditional_chinese_first() {
        // An adapter named "乙太網路 4" is Windows' Traditional Chinese.
        let slots = fallback_slots(&han_order(&langs(&["zh-TW"]), Some(950)));
        assert_eq!(slots[0].choices[0].file, "msjh.ttc");
        let names: Vec<&str> = slots.iter().map(|s| s.name).collect();
        assert_eq!(&names[..4], ["han_traditional", "han_simplified", "han_japanese", "han_korean"]);
    }

    /// egui's own font files, which these tests stand in for Windows' with.
    fn egui_font(name: &str) -> &'static [u8] {
        let defs = FontDefinitions::default();
        match defs.font_data.get(name).map(|d| d.font.clone()) {
            Some(Cow::Borrowed(b)) => b,
            other => panic!("{name}: {other:?}"),
        }
    }

    /// A collection (`.ttc`) of `fonts`, each a plain font file: a header
    /// pointing at each font's table directory, the table offsets moved by
    /// where each font now starts.
    fn collection(fonts: &[&[u8]]) -> &'static [u8] {
        let be32 = |b: &[u8], at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let header = 12 + 4 * fonts.len();
        let mut starts = Vec::new();
        let mut at = header;
        for f in fonts {
            at = (at + 3) & !3;
            starts.push(at);
            at += f.len();
        }
        let mut out = vec![0u8; at];
        out[..4].copy_from_slice(b"ttcf");
        out[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        out[8..12].copy_from_slice(&(fonts.len() as u32).to_be_bytes());
        for (i, (f, &start)) in fonts.iter().zip(&starts).enumerate() {
            out[12 + 4 * i..16 + 4 * i].copy_from_slice(&(start as u32).to_be_bytes());
            out[start..start + f.len()].copy_from_slice(f);
            let tables = u16::from_be_bytes([f[4], f[5]]) as usize;
            for t in 0..tables {
                let rec = start + 12 + 16 * t;
                let moved = be32(&out, rec + 8) + start as u32;
                out[rec + 8..rec + 12].copy_from_slice(&moved.to_be_bytes());
            }
        }
        Box::leak(out.into_boxed_slice())
    }

    fn names_of(bytes: &[u8], index: u32) -> Vec<String> {
        family_names(&ttf_parser::Face::parse(bytes, index).unwrap())
    }

    #[test]
    fn a_face_in_a_collection_is_chosen_by_its_family_name() {
        let (hack, ubuntu) = (egui_font("Hack"), egui_font("Ubuntu-Light"));
        let hack_name = names_of(hack, 0)[0].clone();
        let ubuntu_name = names_of(ubuntu, 0)[0].clone();
        assert_ne!(hack_name, ubuntu_name);
        let ttc = collection(&[ubuntu, hack]);
        assert_eq!(ttf_parser::fonts_in_collection(ttc), Some(2));
        assert_eq!(choose_face(ttc, &[hack_name.as_str()]), Ok(1));
        assert_eq!(choose_face(ttc, &[ubuntu_name.as_str()]), Ok(0));
        // Case doesn't matter; an earlier wanted name wins over a later one.
        assert_eq!(choose_face(ttc, &[&hack_name.to_uppercase()]), Ok(1));
        assert_eq!(choose_face(ttc, &["Not There", &ubuntu_name, &hack_name]), Ok(0));
        // A collection without the wanted face is refused, never face 0.
        let err = choose_face(ttc, &["Microsoft JhengHei UI"]).unwrap_err();
        assert!(err.contains("no face named"), "{err}");
        // A plain file has one face, whatever it is called.
        assert_eq!(choose_face(hack, &["Microsoft JhengHei UI"]), Ok(0));
    }

    #[test]
    fn a_file_egui_could_not_read_is_refused() {
        let hack = egui_font("Hack");
        for bad in [&hack[..12], b"not a font at all", &[][..], &collection(&[hack])[..30]] {
            assert!(choose_face(bad, &["Hack"]).is_err(), "{} bytes", bad.len());
        }
        // Whatever the cut, the verdict is egui's own parser's; a file it
        // accepts can be drawn with (missing tables give no glyphs, never a
        // stop).
        for len in (0..hack.len()).step_by(997) {
            let cut: &'static [u8] = Box::leak(hack[..len].to_vec().into_boxed_slice());
            let ours = choose_face(cut, &["Hack"]).is_ok();
            assert_eq!(ours, ttf_parser::Face::parse(cut, 0).is_ok(), "{len} bytes");
            if ours {
                let mut defs = FontDefinitions::default();
                defs.font_data.insert("cut".into(), FontData::from_static(cut));
                defs.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "cut".into());
                let ctx = egui::Context::default();
                ctx.set_fonts(defs);
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| ui.label("Text drawn with a cut font ⏵"));
                });
            }
        }
    }

    /// A fonts folder of egui's own files under Windows' names: a
    /// collection without the face wanted from it, a file cut short, a
    /// plain font under a collection's name, and most files missing.
    fn folder(file: &str) -> Option<&'static [u8]> {
        match file {
            "segoeui.ttf" => Some(egui_font("Ubuntu-Light")),
            "seguisb.ttf" => Some(egui_font("Hack")),
            "seguisym.ttf" => Some(egui_font("NotoEmoji-Regular")),
            "msjh.ttc" => Some(collection(&[egui_font("Hack"), egui_font("Ubuntu-Light")])),
            "YuGothM.ttc" => Some(egui_font("emoji-icon-font")),
            "malgun.ttf" => Some(&egui_font("Hack")[..12]),
            _ => None,
        }
    }

    /// The families as they were before the fallbacks: egui's defaults,
    /// Segoe UI first and Semibold second in the proportional family,
    /// Symbol last in both.
    fn todays_families() -> (Vec<String>, Vec<String>) {
        let defaults = FontDefinitions::default();
        let mut proportional = defaults.families[&FontFamily::Proportional].clone();
        let mut monospace = defaults.families[&FontFamily::Monospace].clone();
        proportional.insert(0, "segoe_ui".into());
        proportional.insert(1, "segoe_ui_semibold".into());
        proportional.push("segoe_ui_symbol".into());
        monospace.push("segoe_ui_symbol".into());
        (proportional, monospace)
    }

    #[test]
    fn todays_fonts_keep_their_places_and_everything_new_comes_after() {
        let han = han_order(&langs(&["zh-TW"]), None);
        let (defs, loaded) = definitions(&han, &mut folder);
        let proportional = &defs.families[&FontFamily::Proportional];
        let monospace = &defs.families[&FontFamily::Monospace];
        let (today_p, today_m) = todays_families();
        assert_eq!(proportional[..today_p.len()], today_p[..]);
        assert_eq!(monospace[..today_m.len()], today_m[..]);
        // After Segoe UI Symbol, the same fonts in both families: here only
        // the one stand-in that loads.
        assert_eq!(proportional[today_p.len()..], ["han_japanese".to_string()]);
        assert_eq!(monospace[today_m.len()..], ["han_japanese".to_string()]);
        // The collection without JhengHei (not face 0 instead) and the file
        // cut short are skipped; missing files are no error.
        let skipped: Vec<&str> = loaded.skipped.iter().map(|(f, _)| *f).collect();
        assert_eq!(skipped, ["msjh.ttc", "malgun.ttf"]);
        assert!(!defs.font_data.contains_key("han_traditional") && !defs.font_data.contains_key("han_korean"));
        // The bytes are lent to egui as they are: no copy.
        let lent = &defs.font_data["han_japanese"];
        assert!(matches!(&lent.font, Cow::Borrowed(b) if b.as_ptr() == egui_font("emoji-icon-font").as_ptr()));
        // With no fonts folder at all, egui's defaults as they are.
        let (none, loaded) = definitions(&han, &mut |_| None);
        assert_eq!(none.families, FontDefinitions::default().families);
        assert!(loaded.used.is_empty() && loaded.skipped.is_empty());
    }

    #[test]
    fn a_frame_with_the_stack_draws_without_stopping() {
        let (defs, _) = definitions(&han_order(&langs(&["zh-TW"]), Some(950)), &mut folder);
        let ctx = egui::Context::default();
        ctx.set_fonts(defs);
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("書房的 MacBook Pro (2) (AirPlay)");
                    ui.monospace("乙太網路 4");
                });
            });
        }
        assert!(ctx.fonts(|f| f.has_glyphs(&egui::FontId::proportional(14.0), "MacBook Pro (2)")));
    }

    /// On Windows (the project's CI runner): the real fonts folder. Prints
    /// each file's size and face names, builds the stack, draws a frame,
    /// and checks a sample of each script whose font loaded. Files that
    /// aren't there are normal (Server and N editions lack some).
    #[cfg(windows)]
    #[test]
    fn the_real_fonts_folder_covers_each_script_whose_font_is_there() {
        let dir = windows::fonts_folder();
        let slots: Vec<&Slot> =
            [&SEGOE_UI, &SEGOE_UI_SEMIBOLD, &SEGOE_UI_SYMBOL].into_iter().chain(fallback_slots(&DEFAULT_HAN_ORDER)).collect();
        for slot in &slots {
            for choice in slot.choices {
                match std::fs::read(dir.join(choice.file)) {
                    Ok(bytes) => {
                        let faces = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
                        let names: Vec<Vec<String>> = (0..faces)
                            .map(|i| ttf_parser::Face::parse(&bytes, i).map(|f| family_names(&f)).unwrap_or_default())
                            .collect();
                        println!("{}: {} bytes, faces {names:?}", choice.file, bytes.len());
                    }
                    Err(e) => println!("{}: not loaded ({e})", choice.file),
                }
            }
        }
        println!("languages {:?}, code page {:?}", windows::languages(), windows::ansi_code_page());
        let mut load = |file: &str| windows::map_read_only(&dir.join(file)).ok();
        let (defs, loaded) = definitions(&DEFAULT_HAN_ORDER, &mut load);
        println!("used {:?}\nskipped {:?}", loaded.used, loaded.skipped);
        assert!(loaded.used.iter().any(|(s, _)| *s == "segoe_ui"), "Segoe UI is on every Windows");
        let ctx = egui::Context::default();
        ctx.set_fonts(defs);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| ui.label("書房的 MacBook Pro (2) (AirPlay)"));
        });
        let samples = [
            ("han_traditional", "乙太網路 4"),
            ("han_simplified", "简体中文"),
            ("han_japanese", "リビング"),
            ("han_korean", "거실"),
            ("han_rare", "𠀀𪚥"),
            ("indic", "हिन्दी தமிழ்"),
            ("thai_lao_khmer", "ภาษาไทย"),
            ("ethiopic", "ኢትዮጵያ"),
            ("cherokee_syllabics", "ᏣᎳᎩ"),
            ("myanmar", "မြန်မာ"),
            ("tibetan", "བོད"),
            ("mongolian", "ᠮᠣᠩᠭᠣᠯ"),
            ("yi", "ꆈꌠ"),
        ];
        for (slot, text) in samples {
            let used = loaded.used.iter().any(|(s, _)| *s == slot);
            for id in [egui::FontId::proportional(14.0), egui::FontId::monospace(13.0)] {
                let drawn = ctx.fonts(|f| f.has_glyphs(&id, text));
                println!("{slot}: {text} {}", if drawn { "drawn" } else { "not drawn" });
                assert!(!used || drawn, "{slot} loaded but {text:?} isn't drawn in {id:?}");
            }
        }
        if loaded.used.iter().any(|(s, _)| s.starts_with("han_")) {
            let id = egui::FontId::proportional(14.0);
            assert!(ctx.fonts(|f| f.has_glyphs(&id, "書房的 MacBook Pro (2) (AirPlay)")));
        }
    }
}
