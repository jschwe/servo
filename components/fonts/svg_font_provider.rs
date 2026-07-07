/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use app_units::Au;
use fonts_traits::{
    FontDescriptor, FontIdentifier, FontTemplateRef, FontTemplateRefMethods, SystemFontServiceProxy,
};
use icu_locid::subtags::Language;
use net_traits::image_cache::{
    SvgFontData, SvgFontFamily, SvgFontProvider, SvgFontQuery, SvgFontStyle,
};
use parking_lot::Mutex;
use read_fonts::TableProvider;
use rustc_hash::FxHashMap;
use style::computed_values::font_optical_sizing::T as FontOpticalSizing;
use style::computed_values::font_variant_caps::T as FontVariantCaps;
use style::values::computed::font::{
    FamilyName, FontFamilyNameSyntax, GenericFontFamily, SingleFontFamily,
};
use style::values::computed::{FontStretch, FontStyle, FontSynthesis, FontWeight};

use crate::{FallbackFontSelectionOptions, fallback_font_families};

/// A [`SvgFontProvider`] that resolves fonts through the `SystemFontService`,
/// so that rasterizing text in vector images does not require a separate
/// system font scan.
pub struct SvgFontProviderImpl {
    system_font_service: Arc<SystemFontServiceProxy>,
    data_cache: Mutex<FxHashMap<FontIdentifier, Option<SvgFontData>>>,
}

impl SvgFontProviderImpl {
    pub fn new(system_font_service: Arc<SystemFontServiceProxy>) -> Self {
        Self {
            system_font_service,
            data_cache: Mutex::default(),
        }
    }

    fn font_data_for_template(&self, template: FontTemplateRef) -> Option<SvgFontData> {
        let identifier = template.identifier();
        self.data_cache
            .lock()
            .entry(identifier.clone())
            .or_insert_with(|| {
                let FontIdentifier::Local(local_identifier) = &identifier else {
                    return None;
                };
                let data_and_index = local_identifier.font_data_and_index()?;
                Some(SvgFontData {
                    key: format!("{identifier:?}"),
                    data: Arc::new(data_and_index.data),
                    index: data_and_index.index,
                })
            })
            .clone()
    }

    fn font_descriptor(query: &SvgFontQuery) -> FontDescriptor {
        FontDescriptor {
            weight: FontWeight::from_float(query.weight as f32),
            stretch: FontStretch::from_percentage(query.stretch_percentage / 100.),
            style: match query.style {
                SvgFontStyle::Normal => FontStyle::NORMAL,
                SvgFontStyle::Italic => FontStyle::ITALIC,
                SvgFontStyle::Oblique => FontStyle::OBLIQUE,
            },
            variant: FontVariantCaps::Normal,
            pt_size: Au::from_f32_px(16.),
            variation_settings: Vec::new(),
            synthesis_weight: FontSynthesis::Auto,
            optical_sizing: FontOpticalSizing::Auto,
        }
    }
}

impl SvgFontProvider for SvgFontProviderImpl {
    fn select_font(&self, query: &SvgFontQuery) -> Option<SvgFontData> {
        let descriptor = Self::font_descriptor(query);
        let families = query
            .families
            .iter()
            .map(|family| match family {
                SvgFontFamily::Named(name) => SingleFontFamily::FamilyName(FamilyName {
                    name: name.as_str().into(),
                    syntax: FontFamilyNameSyntax::Quoted,
                }),
                SvgFontFamily::Serif => SingleFontFamily::Generic(GenericFontFamily::Serif),
                SvgFontFamily::SansSerif => SingleFontFamily::Generic(GenericFontFamily::SansSerif),
                SvgFontFamily::Cursive => SingleFontFamily::Generic(GenericFontFamily::Cursive),
                SvgFontFamily::Fantasy => SingleFontFamily::Generic(GenericFontFamily::Fantasy),
                SvgFontFamily::Monospace => SingleFontFamily::Generic(GenericFontFamily::Monospace),
            })
            .chain(std::iter::once(SingleFontFamily::Generic(
                GenericFontFamily::None,
            )));

        for family in families {
            for template in self
                .system_font_service
                .find_matching_font_templates(Some(&descriptor), &family)
            {
                if let Some(font_data) = self.font_data_for_template(template) {
                    return Some(font_data);
                }
            }
        }
        None
    }

    fn select_fallback(
        &self,
        character: char,
        exclude_keys: &[String],
        query: &SvgFontQuery,
    ) -> Option<SvgFontData> {
        let descriptor = Self::font_descriptor(query);
        let options = FallbackFontSelectionOptions::new(character, None, Language::UND);
        for family_name in fallback_font_families(options) {
            let family = SingleFontFamily::FamilyName(FamilyName {
                name: family_name.into(),
                syntax: FontFamilyNameSyntax::Quoted,
            });
            for template in self
                .system_font_service
                .find_matching_font_templates(Some(&descriptor), &family)
            {
                let Some(font_data) = self.font_data_for_template(template) else {
                    continue;
                };
                if exclude_keys.contains(&font_data.key) ||
                    !font_supports_character(&font_data, character)
                {
                    continue;
                }
                return Some(font_data);
            }
        }
        None
    }
}

fn font_supports_character(font_data: &SvgFontData, character: char) -> bool {
    read_fonts::FontRef::from_index((*font_data.data).as_ref(), font_data.index)
        .ok()
        .and_then(|font| font.cmap().ok())
        .and_then(|cmap| cmap.map_codepoint(character))
        .is_some()
}
