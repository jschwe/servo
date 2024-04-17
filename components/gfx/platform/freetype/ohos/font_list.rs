/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::Path;

use log::warn;
use serde::{Deserialize, Serialize};
use style::Atom;
use ucd::{Codepoint, UnicodeBlock};

use crate::text::util::is_cjk;

lazy_static::lazy_static! {
    static ref FONT_LIST: FontList = FontList::new();
}

/// An identifier for a local font on Android systems.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct LocalFontIdentifier {
    /// The path to the font.
    pub path: Atom,
}

struct Font {
    filename: String,
    weight: Option<i32>,
}

struct FontFamily {
    name: String,
    fonts: Vec<Font>,
}

struct FontAlias {
    from: String,
    to: String,
    weight: Option<i32>,
}

struct FontList {
    families: Vec<FontFamily>,
    aliases: Vec<FontAlias>,
}

impl FontList {
    fn new() -> FontList {
        FontList {
            families: Self::fallback_font_families(),
            aliases: Vec::new(),
        }
    }

    // Fonts expected to exist in Android devices.
    // Only used in the unlikely case where no font xml mapping files are found.
    fn fallback_font_families() -> Vec<FontFamily> {
        let alternatives = [
            ("sans-serif", "HarmonyOS_Sans_SC_Regular.ttf"),
            // ("Droid Sans", "DroidSans.ttf"),
            // (
            //     "Lomino",
            //     "/system/etc/ml/kali/Fonts/Lomino/Medium/LominoUI_Md.ttf",
            // ),
        ];

        alternatives
            .iter()
            .filter(|item| Path::new(&Self::font_absolute_path(item.1)).exists())
            .map(|item| FontFamily {
                name: item.0.into(),
                fonts: vec![Font {
                    filename: item.1.into(),
                    weight: None,
                }],
            })
            .collect()
    }

    // OHOS fonts are located in /system/fonts
    fn font_absolute_path(filename: &str) -> String {
        if filename.starts_with("/") {
            String::from(filename)
        } else {
            format!("/system/fonts/{}", filename)
        }
    }

    fn find_family(&self, name: &str) -> Option<&FontFamily> {
        self.families.iter().find(|f| f.name == name)
    }

    fn find_alias(&self, name: &str) -> Option<&FontAlias> {
        self.aliases.iter().find(|f| f.from == name)
    }


    // fn find_attrib(name: &str, attrs: &[Attribute]) -> Option<String> {
    //     attrs
    //         .iter()
    //         .find(|attr| attr.name.local_name == name)
    //         .map(|attr| attr.value.clone())
    // }
    //
    // fn text_content(nodes: &[Node]) -> Option<String> {
    //     nodes.get(0).and_then(|child| match child {
    //         Node::Text(contents) => Some(contents.clone()),
    //         Node::Element { .. } => None,
    //     })
    // }
    //
    // fn collect_contents_with_tag(nodes: &[Node], tag: &str, out: &mut Vec<String>) {
    //     for node in nodes {
    //         if let Node::Element { name, children, .. } = node {
    //             if name.local_name == tag {
    //                 if let Some(content) = Self::text_content(children) {
    //                     out.push(content);
    //                 }
    //             }
    //         }
    //     }
    // }
}

// Functions used by FontCacheThread
pub fn for_each_available_family<F>(mut callback: F)
    where
        F: FnMut(String),
{
    for family in &FONT_LIST.families {
        callback(family.name.clone());
    }
    for alias in &FONT_LIST.aliases {
        callback(alias.from.clone());
    }
}

pub fn for_each_variation<F>(family_name: &str, mut callback: F)
    where
        F: FnMut(LocalFontIdentifier),
{
    let mut produce_font = |font: &Font| {
        callback(LocalFontIdentifier {
            path: Atom::from(FontList::font_absolute_path(&font.filename)),
        })
    };

    if let Some(family) = FONT_LIST.find_family(family_name) {
        for font in &family.fonts {
            produce_font(font);
        }
        return;
    }

    if let Some(alias) = FONT_LIST.find_alias(family_name) {
        if let Some(family) = FONT_LIST.find_family(&alias.to) {
            for font in &family.fonts {
                match (alias.weight, font.weight) {
                    (None, _) => produce_font(font),
                    (Some(w1), Some(w2)) if w1 == w2 => produce_font(font),
                    _ => {},
                }
            }
        }
    }
}

// TODO: Font config file available at /system/fonts/visibility_list.json, but unsure if we
// can access it from inside our sandbox!
// File also seems to be missing on my system....

pub fn system_default_family(generic_name: &str) -> Option<String> {
    if let Some(family) = FONT_LIST.find_family(&generic_name) {
        Some(family.name.clone())
    } else if let Some(alias) = FONT_LIST.find_alias(&generic_name) {
        Some(alias.from.clone())
    } else {
        //  First font defined in the fonts.xml is the default on Android.
        FONT_LIST.families.get(0).map(|family| family.name.clone())
    }
}

// Based on gfxAndroidPlatform::GetCommonFallbackFonts() in Gecko
pub fn fallback_font_families(codepoint: Option<char>) -> Vec<&'static str> {
    let mut families = vec![];

    if let Some(block) = codepoint.and_then(|c| c.block()) {
        match block {
            // UnicodeBlock::Armenian => {
            //     families.push("Droid Sans Armenian");
            // },

            UnicodeBlock::Hebrew => {
                families.push("Noto Sans Hebrew");
            },

            UnicodeBlock::Arabic => {
                families.push("HarmonyOS Sans Naskh Arabic");
            },

            UnicodeBlock::Devanagari => {
                families.push("Noto Sans Devanagari");
            },

            UnicodeBlock::Tamil => {
                families.push("Noto Sans Tamil");
            },

            UnicodeBlock::Thai => {
                families.push("Noto Sans Thai");
            },

            UnicodeBlock::Georgian | UnicodeBlock::GeorgianSupplement => {
                families.push("Noto Sans Georgian");
            },

            UnicodeBlock::Ethiopic | UnicodeBlock::EthiopicSupplement => {
                families.push("Noto Sans Ethiopic");
            },

            _ => {
                if is_cjk(codepoint.unwrap()) {
                    families.push("MotoyaLMaru");
                    families.push("Noto Sans JP");
                    families.push("Noto Sans KR");
                }
            },
        }
    }

    families.push("HarmonyOS Sans SC Regular");
    families
}

pub static SANS_SERIF_FONT_FAMILY: &'static str = "sans-serif";
