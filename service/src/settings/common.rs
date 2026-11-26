//! Words and values the views and the dialogs share.

use crate::model::text::{Lang, Number};

use super::words::w;

/// *−2 dB*, *+1 dB*, *0 dB*.
pub fn speaker_db(lang: Lang, v: f32) -> String {
    let n = (v * 10.0).round() as i64;
    let num = Number { value: n.abs(), scale: 1 }.normalized();
    let digits = lang.number(num);
    if n > 0 {
        w!(lang, SetDbPlus, n = &digits)
    } else if n < 0 {
        w!(lang, SetDbMinus, n = &digits)
    } else {
        w!(lang, SetDbZero)
    }
}

/// *482 917*: a six-digit code in two groups of three.
pub fn spaced_code(code: &str) -> String {
    let digits: Vec<char> = code.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() == 6 {
        let a: String = digits[..3].iter().collect();
        let b: String = digits[3..].iter().collect();
        format!("{a}\u{2002}{b}")
    } else {
        code.to_string()
    }
}

/// A band's frequency as its label says it: *31*, *1k*.
pub fn hz_label(hz: f64) -> String {
    if hz >= 1000.0 {
        format!("{}k", (hz / 1000.0).round() as i64)
    } else {
        format!("{}", hz.round() as i64)
    }
}

/// A preset of the equalizer by its name.
pub fn preset_name(lang: Lang, name: &str) -> String {
    match name {
        "Flat" => w!(lang, SetPresetFlat),
        "Bass boost" => w!(lang, SetPresetBassBoost),
        "Bass cut" => w!(lang, SetPresetBassCut),
        "Treble boost" => w!(lang, SetPresetTrebleBoost),
        "Vocal" => w!(lang, SetPresetVoice),
        "Loudness" => w!(lang, SetPresetLoudness),
        "Small speaker" => w!(lang, SetPresetSmallSpeaker),
        other => other.to_string(),
    }
}
