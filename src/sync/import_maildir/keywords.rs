/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::Path;

use mail_parser::mailbox::maildir::Flag;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Translation {
    pub keywords: Vec<String>,
    pub has_trashed_flag: bool,
}

pub fn translate_flags(flags: &[Flag], include_deleted: bool) -> Translation {
    let mut out: Vec<String> = Vec::new();
    let mut has_trashed = false;
    for f in flags {
        match f {
            Flag::Passed => push_unique(&mut out, "$forwarded"),
            Flag::Replied => push_unique(&mut out, "$answered"),
            Flag::Seen => push_unique(&mut out, "$seen"),
            Flag::Draft => push_unique(&mut out, "$draft"),
            Flag::Flagged => push_unique(&mut out, "$flagged"),
            Flag::Trashed => {
                has_trashed = true;
                if include_deleted {
                    push_unique(&mut out, "$deleted");
                }
            }
        }
    }
    out.sort();
    Translation {
        keywords: out,
        has_trashed_flag: has_trashed,
    }
}

/// The translation with a message's own keywords added: those Dovecot
/// keeps as the letters `a` to `z`.
pub fn with_custom(mut translation: Translation, custom: &[String]) -> Translation {
    for k in custom {
        push_unique(&mut translation.keywords, k);
    }
    translation.keywords.sort();
    translation
}

/// What the keyword letters `a` to `z` stand for in one folder, from its
/// `dovecot-keywords` file: a line `0 $Junk` names `a`, `1 work` names `b`.
pub fn read_keyword_names(folder_path: &Path) -> Vec<Option<String>> {
    let mut names = vec![None; 26];
    let Ok(text) = std::fs::read_to_string(folder_path.join("dovecot-keywords")) else {
        return names;
    };
    for line in text.lines() {
        let Some((index, name)) = line.split_once(' ') else {
            continue;
        };
        let name = name.trim();
        if let Ok(i) = index.parse::<usize>()
            && i < names.len()
            && !name.is_empty()
        {
            names[i] = Some(name.to_owned());
        }
    }
    names
}

pub fn custom_keywords_from_filename(filename: &str, names: &[Option<String>]) -> Vec<String> {
    let Some(part) = filename.rsplit_once("2,").map(|(_, s)| s) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ch in part.bytes().take_while(u8::is_ascii_alphanumeric) {
        if ch.is_ascii_lowercase()
            && let Some(Some(name)) = names.get((ch - b'a') as usize)
        {
            push_unique(&mut out, name);
        }
    }
    out
}

fn push_unique(out: &mut Vec<String>, k: &str) {
    if !out.iter().any(|x| x == k) {
        out.push(k.to_owned());
    }
}

pub fn flags_from_filename(filename: &str) -> Vec<Flag> {
    let Some(part) = filename.rsplit_once("2,").map(|(_, s)| s) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &ch in part.as_bytes() {
        match ch {
            b'P' => out.push(Flag::Passed),
            b'R' => out.push(Flag::Replied),
            b'S' => out.push(Flag::Seen),
            b'T' => out.push(Flag::Trashed),
            b'D' => out.push(Flag::Draft),
            b'F' => out.push(Flag::Flagged),
            other => {
                if !other.is_ascii_alphanumeric() {
                    break;
                }
            }
        }
    }
    out
}

pub fn unique_id_from_filename(filename: &str) -> &str {
    match filename.split_once(':') {
        Some((unique, _)) => unique,
        None => filename,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dovecot_keyword_letters_name_keywords() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("dovecot-keywords"),
            "0 $Junk\n1 project-x\nbad\n30 x\n",
        )
        .unwrap();
        let names = read_keyword_names(td.path());
        assert_eq!(names[0].as_deref(), Some("$Junk"));
        assert_eq!(names[1].as_deref(), Some("project-x"));
        assert_eq!(
            custom_keywords_from_filename("1.M1.h,S=10:2,FSab", &names),
            vec!["$Junk", "project-x"]
        );
        assert!(custom_keywords_from_filename("1.M1.h:2,Sc", &names).is_empty());
        let t = with_custom(translate_flags(&[Flag::Seen], false), &["$Junk".to_owned()]);
        assert_eq!(t.keywords, vec!["$Junk", "$seen"]);
    }

    #[test]
    fn a_folder_without_dovecot_keywords_names_none() {
        let td = tempfile::tempdir().unwrap();
        assert!(read_keyword_names(td.path()).iter().all(Option::is_none));
    }

    #[test]
    fn empty_flags_yields_empty_keywords() {
        let t = translate_flags(&[], false);
        assert!(t.keywords.is_empty());
        assert!(!t.has_trashed_flag);
    }

    #[test]
    fn each_flag_maps_to_expected_jmap_keyword() {
        let t = translate_flags(
            &[
                Flag::Passed,
                Flag::Replied,
                Flag::Seen,
                Flag::Draft,
                Flag::Flagged,
            ],
            false,
        );
        let mut expected = vec!["$forwarded", "$answered", "$seen", "$draft", "$flagged"];
        expected.sort();
        let expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
        assert_eq!(t.keywords, expected);
        assert!(!t.has_trashed_flag);
    }

    #[test]
    fn trashed_dropped_by_default_marks_flag() {
        let t = translate_flags(&[Flag::Trashed, Flag::Seen], false);
        assert!(t.has_trashed_flag);
        assert_eq!(t.keywords, vec!["$seen".to_owned()]);
    }

    #[test]
    fn trashed_with_include_deleted_adds_dollar_deleted() {
        let t = translate_flags(&[Flag::Trashed, Flag::Seen], true);
        assert!(t.has_trashed_flag);
        let mut expected = vec!["$seen", "$deleted"];
        expected.sort();
        let expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
        assert_eq!(t.keywords, expected);
    }

    #[test]
    fn order_independent_keywords_are_stable() {
        let a = translate_flags(&[Flag::Seen, Flag::Flagged], false);
        let b = translate_flags(&[Flag::Flagged, Flag::Seen], false);
        assert_eq!(a.keywords, b.keywords);
    }

    #[test]
    fn duplicate_flags_dedup() {
        let t = translate_flags(&[Flag::Seen, Flag::Seen, Flag::Seen], false);
        assert_eq!(t.keywords, vec!["$seen".to_owned()]);
    }

    #[test]
    fn unique_id_strips_at_first_colon() {
        assert_eq!(
            unique_id_from_filename("1739471123.M001P01234V0.host:2,RS"),
            "1739471123.M001P01234V0.host"
        );
    }

    #[test]
    fn unique_id_no_colon_is_whole_basename() {
        assert_eq!(
            unique_id_from_filename("1739471123.M001P0.host"),
            "1739471123.M001P0.host"
        );
    }

    #[test]
    fn unique_id_strips_legacy_one_form() {
        assert_eq!(
            unique_id_from_filename("1234.M5.host:1,experimental"),
            "1234.M5.host"
        );
    }

    #[test]
    fn flags_from_filename_recognises_full_set() {
        let mut flags = flags_from_filename("name:2,DFPRST");
        flags.sort();
        let mut want = vec![
            Flag::Draft,
            Flag::Flagged,
            Flag::Passed,
            Flag::Replied,
            Flag::Seen,
            Flag::Trashed,
        ];
        want.sort();
        assert_eq!(flags, want);
    }

    #[test]
    fn flags_from_filename_no_info_section_yields_empty() {
        assert!(flags_from_filename("plain").is_empty());
        assert!(flags_from_filename("name:").is_empty());
        assert!(flags_from_filename("name:1,X").is_empty());
    }

    #[test]
    fn flags_from_filename_dovecot_extension_metadata_passes_through() {
        let mut flags = flags_from_filename("uid,S=1234,W=1300:2,RS");
        flags.sort();
        let mut want = vec![Flag::Replied, Flag::Seen];
        want.sort();
        assert_eq!(flags, want);
    }

    #[test]
    fn flags_from_filename_unknown_alpha_chars_skipped() {
        let mut flags = flags_from_filename("uid:2,SaT");
        flags.sort();
        let mut want = vec![Flag::Seen, Flag::Trashed];
        want.sort();
        assert_eq!(flags, want);
    }
}
