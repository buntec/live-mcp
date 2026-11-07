//! Pitch-class and note names, spelled to suit the key.

use super::key::Key;

const SHARPS: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
const FLATS: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"];
/// For C major / A minor and when there is no key: the commonest spelling of each black key.
const NEUTRAL: [&str; 12] = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];

pub type Spelling = [&'static str; 12];

/// Sharps for sharp keys, flats for flat keys. Minor keys spell like their relative major, except
/// that the raised 7th (the leading tone) is a sharp: G# in A minor, F# in G minor.
pub fn spelling(key: Option<Key>) -> Spelling {
    let Some(key) = key else { return NEUTRAL };
    let major_tonic = if key.minor { (key.tonic + 3) % 12 } else { key.tonic };
    let mut names = match major_tonic {
        0 => NEUTRAL,
        7 | 2 | 9 | 4 | 11 | 6 => SHARPS,
        _ => FLATS, // F, Bb, Eb, Ab, Db
    };
    if key.minor {
        let leading = ((key.tonic + 11) % 12) as usize;
        names[leading] = SHARPS[leading];
    }
    names
}

/// A MIDI note as Live names it: middle C (60) is C3.
pub fn note_name(pitch: u8, spelling: Spelling) -> String {
    format!("{}{}", spelling[(pitch % 12) as usize], pitch as i32 / 12 - 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_choose_sharps_or_flats() {
        let key = |tonic, minor| Some(Key { tonic, minor });
        assert_eq!(spelling(key(2, false))[6], "F#"); // D major
        assert_eq!(spelling(key(5, false))[10], "Bb"); // F major
        assert_eq!(spelling(key(0, true))[3], "Eb"); // C minor, like Eb major
        assert_eq!(spelling(key(4, true))[6], "F#"); // E minor, like G major
        assert_eq!(spelling(None)[8], "Ab");
        assert_eq!(spelling(key(9, true))[8], "G#"); // A minor's leading tone
        assert_eq!(spelling(key(7, true))[6], "F#"); // G minor's, in a flat key
    }

    #[test]
    fn notes_use_lives_octaves() {
        assert_eq!(note_name(60, spelling(None)), "C3");
        assert_eq!(note_name(33, spelling(None)), "A0");
        assert_eq!(note_name(22, FLATS), "Bb-1");
    }
}
